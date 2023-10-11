// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logic related to opening a [`Catalog`].
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::str::FromStr;
use std::sync::{atomic, Arc};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use tracing::{info, warn};
use uuid::Uuid;

use mz_catalog::builtin::{
    Builtin, Fingerprint, BUILTINS, BUILTIN_CLUSTERS, BUILTIN_PREFIXES, BUILTIN_ROLES,
};
use mz_catalog::objects::{SystemObjectDescription, SystemObjectUniqueIdentifier};
use mz_catalog::SystemObjectMapping;
use mz_compute_client::controller::ComputeReplicaConfig;
use mz_compute_client::logging::LogVariant;
use mz_controller::clusters::{ReplicaConfig, ReplicaLogging};
use mz_controller_types::ClusterId;
use mz_ore::cast::CastFrom;
use mz_ore::collections::CollectionExt;
use mz_ore::now::to_datetime;
use mz_pgrepr::oid::FIRST_USER_OID;
use mz_repr::adt::mz_acl_item::PrivilegeMap;
use mz_repr::role_id::RoleId;
use mz_repr::GlobalId;
use mz_sql::catalog::{
    CatalogError as SqlCatalogError, CatalogItem as SqlCatalogItem, CatalogItemType, CatalogType,
};
use mz_sql::func::OP_IMPLS;
use mz_sql::names::{
    ItemQualifiers, QualifiedItemName, QualifiedSchemaName, ResolvedDatabaseSpecifier, ResolvedIds,
    SchemaId, SchemaSpecifier,
};
use mz_sql::session::user::MZ_SYSTEM_ROLE_ID;
use mz_sql::session::vars::{
    OwnedVarInput, SystemVars, Var, VarError, VarInput, CONFIG_HAS_SYNCED_ONCE,
};
use mz_sql::{plan, rbac};
use mz_sql_parser::ast::display::AstDisplay;
use mz_sql_parser::ast::Expr;
use mz_ssh_util::keys::SshKeyPairSet;
use mz_storage_types::sources::Timeline;

use crate::catalog::objects::{
    CatalogEntry, CatalogItem, CommentsMap, DataSourceDesc, Database, DefaultPrivileges, Func, Log,
    Role, Schema, Source, Table, Type,
};
use crate::catalog::{
    is_reserved_name, migrate, BuiltinTableUpdate, Catalog, CatalogPlans, CatalogState, Config,
    Error, ErrorKind, Op, CREATE_SQL_TODO, SYSTEM_CONN_ID,
};
use crate::config::{SynchronizedParameters, SystemParameterFrontend, SystemParameterSyncConfig};
use crate::coord::timestamp_oracle;
use crate::util::ResultExt;
use crate::AdapterError;

#[derive(Debug)]
pub struct BuiltinMigrationMetadata {
    // Used to drop objects on STORAGE nodes
    pub previous_sink_ids: Vec<GlobalId>,
    pub previous_materialized_view_ids: Vec<GlobalId>,
    pub previous_source_ids: Vec<GlobalId>,
    // Used to update in memory catalog state
    pub all_drop_ops: Vec<GlobalId>,
    pub all_create_ops: Vec<(
        GlobalId,
        u32,
        QualifiedItemName,
        RoleId,
        PrivilegeMap,
        CatalogItemRebuilder,
    )>,
    pub introspection_source_index_updates:
        BTreeMap<ClusterId, Vec<(LogVariant, String, GlobalId)>>,
    // Used to update persisted on disk catalog state
    pub migrated_system_object_mappings: BTreeMap<GlobalId, SystemObjectMapping>,
    pub user_drop_ops: Vec<GlobalId>,
    pub user_create_ops: Vec<(GlobalId, SchemaId, String)>,
}

impl BuiltinMigrationMetadata {
    fn new() -> BuiltinMigrationMetadata {
        BuiltinMigrationMetadata {
            previous_sink_ids: Vec::new(),
            previous_materialized_view_ids: Vec::new(),
            previous_source_ids: Vec::new(),
            all_drop_ops: Vec::new(),
            all_create_ops: Vec::new(),
            introspection_source_index_updates: BTreeMap::new(),
            migrated_system_object_mappings: BTreeMap::new(),
            user_drop_ops: Vec::new(),
            user_create_ops: Vec::new(),
        }
    }
}

struct AllocatedBuiltinSystemIds<T> {
    all_builtins: Vec<(T, GlobalId)>,
    new_builtins: Vec<(T, GlobalId)>,
    migrated_builtins: Vec<GlobalId>,
}

#[derive(Debug)]
pub enum CatalogItemRebuilder {
    SystemSource(CatalogItem),
    Object {
        id: GlobalId,
        sql: String,
        is_retained_metrics_object: bool,
        custom_logical_compaction_window: Option<Duration>,
    },
}

impl CatalogItemRebuilder {
    fn new(
        entry: &CatalogEntry,
        id: GlobalId,
        ancestor_ids: &BTreeMap<GlobalId, GlobalId>,
    ) -> Self {
        if id.is_system()
            && (entry.is_table() || entry.is_introspection_source() || entry.is_source())
        {
            Self::SystemSource(entry.item().clone())
        } else {
            let create_sql = entry.create_sql().to_string();
            assert_ne!(create_sql.to_lowercase(), CREATE_SQL_TODO.to_lowercase());
            let mut create_stmt = mz_sql::parse::parse(&create_sql)
                .expect("invalid create sql persisted to catalog")
                .into_element()
                .ast;
            mz_sql::ast::transform::create_stmt_replace_ids(&mut create_stmt, ancestor_ids);
            Self::Object {
                id,
                sql: create_stmt.to_ast_string_stable(),
                is_retained_metrics_object: entry.item().is_retained_metrics_object(),
                custom_logical_compaction_window: entry.item().custom_logical_compaction_window(),
            }
        }
    }

    fn build(self, catalog: &Catalog) -> CatalogItem {
        match self {
            Self::SystemSource(item) => item,
            Self::Object {
                id,
                sql,
                is_retained_metrics_object,
                custom_logical_compaction_window,
            } => catalog
                .parse_item(
                    id,
                    sql.clone(),
                    None,
                    is_retained_metrics_object,
                    custom_logical_compaction_window,
                )
                .unwrap_or_else(|error| panic!("invalid persisted create sql ({error:?}): {sql}")),
        }
    }
}

impl Catalog {
    /// Opens or creates a catalog that stores data at `path`.
    ///
    /// Returns the catalog, metadata about builtin objects that have changed
    /// schemas since last restart, a list of updates to builtin tables that
    /// describe the initial state of the catalog, and the version of the
    /// catalog before any migrations were performed.
    #[tracing::instrument(name = "catalog::open", level = "info", skip_all)]
    pub async fn open(
        config: Config<'_>,
    ) -> Result<
        (
            Catalog,
            BuiltinMigrationMetadata,
            Vec<BuiltinTableUpdate>,
            String,
        ),
        AdapterError,
    > {
        for builtin_role in BUILTIN_ROLES {
            assert!(
                is_reserved_name(builtin_role.name),
                "builtin role {builtin_role:?} must start with one of the following prefixes {}",
                BUILTIN_PREFIXES.join(", ")
            );
        }
        for builtin_cluster in BUILTIN_CLUSTERS {
            assert!(
                is_reserved_name(builtin_cluster.name),
                "builtin cluster {builtin_cluster:?} must start with one of the following prefixes {}",
                BUILTIN_PREFIXES.join(", ")
            );
        }

        let mut catalog = Catalog {
            state: CatalogState {
                database_by_name: BTreeMap::new(),
                database_by_id: BTreeMap::new(),
                entry_by_id: BTreeMap::new(),
                ambient_schemas_by_name: BTreeMap::new(),
                ambient_schemas_by_id: BTreeMap::new(),
                temporary_schemas: BTreeMap::new(),
                clusters_by_id: BTreeMap::new(),
                clusters_by_name: BTreeMap::new(),
                clusters_by_linked_object_id: BTreeMap::new(),
                roles_by_name: BTreeMap::new(),
                roles_by_id: BTreeMap::new(),
                config: mz_sql::catalog::CatalogConfig {
                    start_time: to_datetime((config.now)()),
                    start_instant: Instant::now(),
                    nonce: rand::random(),
                    environment_id: config.environment_id,
                    session_id: Uuid::new_v4(),
                    build_info: config.build_info,
                    timestamp_interval: Duration::from_secs(1),
                    now: config.now.clone(),
                },
                oid_counter: FIRST_USER_OID,
                cluster_replica_sizes: config.cluster_replica_sizes,
                default_storage_cluster_size: config.default_storage_cluster_size,
                availability_zones: config.availability_zones,
                system_configuration: {
                    let mut s = SystemVars::new(config.active_connection_count)
                        .set_unsafe(config.unsafe_mode);
                    if config.all_features {
                        s.enable_all_feature_flags_by_default();
                    }
                    s
                },
                egress_ips: config.egress_ips,
                aws_principal_context: config.aws_principal_context,
                aws_privatelink_availability_zones: config.aws_privatelink_availability_zones,
                http_host_name: config.http_host_name,
                default_privileges: DefaultPrivileges::default(),
                system_privileges: PrivilegeMap::default(),
                comments: CommentsMap::default(),
            },
            plans: CatalogPlans {
                optimized_plan_by_id: Default::default(),
                physical_plan_by_id: Default::default(),
                dataflow_metainfos: BTreeMap::new(),
            },
            transient_revision: 0,
            storage: Arc::new(tokio::sync::Mutex::new(config.storage)),
        };

        // Choose a time at which to boot. This is the time at which we will run
        // internal migrations.
        //
        // This time is usually the current system time, but with protection
        // against backwards time jumps, even across restarts.
        let boot_ts = {
            let mut storage = catalog.storage().await;
            let previous_ts = storage
                .get_timestamp(&Timeline::EpochMilliseconds)
                .await?
                .expect("missing EpochMilliseconds timeline");
            let boot_ts = timestamp_oracle::catalog_oracle::monotonic_now(config.now, previous_ts);
            if !storage.is_read_only() {
                // IMPORTANT: we durably record the new timestamp before using it.
                storage
                    .set_timestamp(&Timeline::EpochMilliseconds, boot_ts)
                    .await?;
            }

            boot_ts
        };

        catalog.create_temporary_schema(&SYSTEM_CONN_ID, MZ_SYSTEM_ROLE_ID)?;

        let databases = catalog.storage().await.get_databases().await?;
        for mz_catalog::Database {
            id,
            name,
            owner_id,
            privileges,
        } in databases
        {
            let oid = catalog.allocate_oid()?;
            catalog.state.database_by_id.insert(
                id.clone(),
                Database {
                    name: name.clone(),
                    id,
                    oid,
                    schemas_by_id: BTreeMap::new(),
                    schemas_by_name: BTreeMap::new(),
                    owner_id,
                    privileges: PrivilegeMap::from_mz_acl_items(privileges),
                },
            );
            catalog
                .state
                .database_by_name
                .insert(name.clone(), id.clone());
        }

        let schemas = catalog.storage().await.get_schemas().await?;
        for mz_catalog::Schema {
            id,
            name,
            database_id,
            owner_id,
            privileges,
        } in schemas
        {
            let oid = catalog.allocate_oid()?;
            let (schemas_by_id, schemas_by_name, database_spec) = match &database_id {
                Some(database_id) => {
                    let db = catalog
                        .state
                        .database_by_id
                        .get_mut(database_id)
                        .expect("catalog out of sync");
                    (
                        &mut db.schemas_by_id,
                        &mut db.schemas_by_name,
                        ResolvedDatabaseSpecifier::Id(*database_id),
                    )
                }
                None => (
                    &mut catalog.state.ambient_schemas_by_id,
                    &mut catalog.state.ambient_schemas_by_name,
                    ResolvedDatabaseSpecifier::Ambient,
                ),
            };
            schemas_by_id.insert(
                id.clone(),
                Schema {
                    name: QualifiedSchemaName {
                        database: database_spec,
                        schema: name.clone(),
                    },
                    id: SchemaSpecifier::Id(id.clone()),
                    oid,
                    items: BTreeMap::new(),
                    functions: BTreeMap::new(),
                    owner_id,
                    privileges: PrivilegeMap::from_mz_acl_items(privileges),
                },
            );
            schemas_by_name.insert(name.clone(), id);
        }

        let roles = catalog.storage().await.get_roles().await?;
        for mz_catalog::Role {
            id,
            name,
            attributes,
            membership,
            vars,
        } in roles
        {
            let oid = catalog.allocate_oid()?;
            catalog.state.roles_by_name.insert(name.clone(), id);
            catalog.state.roles_by_id.insert(
                id,
                Role {
                    name,
                    id,
                    oid,
                    attributes,
                    membership,
                    vars,
                },
            );
        }

        let default_privileges = catalog.storage().await.get_default_privileges().await?;
        for mz_catalog::DefaultPrivilege { object, acl_item } in default_privileges {
            catalog.state.default_privileges.grant(object, acl_item);
        }

        let system_privileges = catalog.storage().await.get_system_privileges().await?;
        catalog.state.system_privileges.grant_all(system_privileges);

        catalog
            .load_system_configuration(
                config.system_parameter_defaults,
                config.system_parameter_sync_config,
                boot_ts,
            )
            .await?;
        // We need to set this variable ASAP, so that builtins get planned with the correct value
        let variable_length_row_encoding = catalog
            .system_config()
            .variable_length_row_encoding_DANGEROUS();
        mz_repr::VARIABLE_LENGTH_ROW_ENCODING
            .store(variable_length_row_encoding, atomic::Ordering::SeqCst);

        let comments = catalog.storage().await.get_comments().await?;
        for mz_catalog::Comment {
            object_id,
            sub_component,
            comment,
        } in comments
        {
            catalog
                .state
                .comments
                .update_comment(object_id, sub_component, Some(comment));
        }

        // Now that LD is loaded, set the intended stash timeout.
        // TODO: Move this into the stash constructor.
        catalog
            .storage()
            .await
            .set_connect_timeout(catalog.system_config().crdb_connect_timeout())
            .await;

        catalog.load_builtin_types().await?;

        let persisted_builtin_ids: BTreeMap<_, _> = catalog
            .storage()
            .await
            .get_system_items()
            .await?
            .into_iter()
            .map(|mapping| (mapping.description, mapping.unique_identifier))
            .collect();
        let AllocatedBuiltinSystemIds {
            all_builtins,
            new_builtins,
            migrated_builtins,
        } = catalog
            .allocate_system_ids(
                BUILTINS::iter()
                    .filter(|builtin| !matches!(builtin, Builtin::Type(_)))
                    .collect(),
                |builtin| {
                    persisted_builtin_ids
                        .get(&SystemObjectDescription {
                            schema_name: builtin.schema().to_string(),
                            object_type: builtin.catalog_item_type(),
                            object_name: builtin.name().to_string(),
                        })
                        .cloned()
                },
            )
            .await?;

        let id_fingerprint_map: BTreeMap<GlobalId, String> = all_builtins
            .iter()
            .map(|(builtin, id)| (*id, builtin.fingerprint()))
            .collect();
        let (builtin_indexes, builtin_non_indexes): (Vec<_>, Vec<_>) = all_builtins
            .into_iter()
            .partition(|(builtin, _)| matches!(builtin, Builtin::Index(_)));

        {
            let span = tracing::span!(tracing::Level::DEBUG, "builtin_non_indexes");
            let _enter = span.enter();
            for (builtin, id) in builtin_non_indexes {
                let schema_id = catalog.state.ambient_schemas_by_name[builtin.schema()];
                let name = QualifiedItemName {
                    qualifiers: ItemQualifiers {
                        database_spec: ResolvedDatabaseSpecifier::Ambient,
                        schema_spec: SchemaSpecifier::Id(schema_id),
                    },
                    item: builtin.name().into(),
                };
                match builtin {
                    Builtin::Log(log) => {
                        let oid = catalog.allocate_oid()?;
                        catalog.state.insert_item(
                            id,
                            oid,
                            name.clone(),
                            CatalogItem::Log(Log {
                                variant: log.variant.clone(),
                                has_storage_collection: false,
                            }),
                            MZ_SYSTEM_ROLE_ID,
                            PrivilegeMap::from_mz_acl_items(vec![
                                rbac::default_builtin_object_privilege(
                                    mz_sql::catalog::ObjectType::Source,
                                ),
                                rbac::owner_privilege(
                                    mz_sql::catalog::ObjectType::Source,
                                    MZ_SYSTEM_ROLE_ID,
                                ),
                            ]),
                        );
                    }

                    Builtin::Table(table) => {
                        let oid = catalog.allocate_oid()?;
                        catalog.state.insert_item(
                            id,
                            oid,
                            name.clone(),
                            CatalogItem::Table(Table {
                                create_sql: CREATE_SQL_TODO.to_string(),
                                desc: table.desc.clone(),
                                defaults: vec![Expr::null(); table.desc.arity()],
                                conn_id: None,
                                resolved_ids: ResolvedIds(BTreeSet::new()),
                                custom_logical_compaction_window: table
                                    .is_retained_metrics_object
                                    .then(|| catalog.state.system_config().metrics_retention()),
                                is_retained_metrics_object: table.is_retained_metrics_object,
                            }),
                            MZ_SYSTEM_ROLE_ID,
                            PrivilegeMap::from_mz_acl_items(vec![
                                rbac::default_builtin_object_privilege(
                                    mz_sql::catalog::ObjectType::Table,
                                ),
                                rbac::owner_privilege(
                                    mz_sql::catalog::ObjectType::Table,
                                    MZ_SYSTEM_ROLE_ID,
                                ),
                            ]),
                        );
                    }
                    Builtin::Index(_) => {
                        unreachable!("handled later once clusters have been created")
                    }
                    Builtin::View(view) => {
                        let item = catalog
                            .parse_item(
                                id,
                                view.sql.into(),
                                None,
                                false,
                                None
                            )
                            .unwrap_or_else(|e| {
                                panic!(
                                    "internal error: failed to load bootstrap view:\n\
                                    {}\n\
                                    error:\n\
                                    {:?}\n\n\
                                    make sure that the schema name is specified in the builtin view's create sql statement.",
                                    view.name, e
                                )
                            });
                        let oid = catalog.allocate_oid()?;
                        catalog.state.insert_item(
                            id,
                            oid,
                            name,
                            item,
                            MZ_SYSTEM_ROLE_ID,
                            PrivilegeMap::from_mz_acl_items(vec![
                                rbac::default_builtin_object_privilege(
                                    mz_sql::catalog::ObjectType::View,
                                ),
                                rbac::owner_privilege(
                                    mz_sql::catalog::ObjectType::View,
                                    MZ_SYSTEM_ROLE_ID,
                                ),
                            ]),
                        );
                    }

                    Builtin::Type(_) => unreachable!("loaded separately"),

                    Builtin::Func(func) => {
                        let oid = catalog.allocate_oid()?;
                        catalog.state.insert_item(
                            id,
                            oid,
                            name.clone(),
                            CatalogItem::Func(Func { inner: func.inner }),
                            MZ_SYSTEM_ROLE_ID,
                            PrivilegeMap::default(),
                        );
                    }

                    Builtin::Source(coll) => {
                        let introspection_type = match &coll.data_source {
                            Some(i) => i.clone(),
                            None => continue,
                        };

                        let oid = catalog.allocate_oid()?;
                        catalog.state.insert_item(
                            id,
                            oid,
                            name.clone(),
                            CatalogItem::Source(Source {
                                create_sql: CREATE_SQL_TODO.to_string(),
                                data_source: DataSourceDesc::Introspection(introspection_type),
                                desc: coll.desc.clone(),
                                timeline: Timeline::EpochMilliseconds,
                                resolved_ids: ResolvedIds(BTreeSet::new()),
                                custom_logical_compaction_window: coll
                                    .is_retained_metrics_object
                                    .then(|| catalog.state.system_config().metrics_retention()),
                                is_retained_metrics_object: coll.is_retained_metrics_object,
                            }),
                            MZ_SYSTEM_ROLE_ID,
                            PrivilegeMap::from_mz_acl_items(vec![
                                rbac::default_builtin_object_privilege(
                                    mz_sql::catalog::ObjectType::Source,
                                ),
                                rbac::owner_privilege(
                                    mz_sql::catalog::ObjectType::Source,
                                    MZ_SYSTEM_ROLE_ID,
                                ),
                            ]),
                        );
                    }
                }
            }
        }

        let clusters = catalog.storage().await.get_clusters().await?;
        let mut cluster_azs = BTreeMap::new();
        for mz_catalog::Cluster {
            id,
            name,
            linked_object_id,
            owner_id,
            privileges,
            config,
        } in clusters
        {
            let introspection_source_index_gids = catalog
                .storage()
                .await
                .get_introspection_source_indexes(id)
                .await?;

            let AllocatedBuiltinSystemIds {
                all_builtins: all_indexes,
                new_builtins: new_indexes,
                ..
            } = catalog
                .allocate_system_ids(BUILTINS::logs().collect(), |log| {
                    introspection_source_index_gids
                        .get(log.name)
                        .cloned()
                        // We migrate introspection sources later so we can hardcode the fingerprint as ""
                        .map(|id| SystemObjectUniqueIdentifier {
                            id,
                            fingerprint: "".to_string(),
                        })
                })
                .await?;

            catalog
                .storage()
                .await
                .set_introspection_source_indexes(
                    new_indexes
                        .iter()
                        .map(|(log, index_id)| (id, log.name, *index_id))
                        .collect(),
                )
                .await?;

            if let mz_catalog::ClusterVariant::Managed(managed) = &config.variant {
                cluster_azs.insert(id, managed.availability_zones.clone());
            }

            catalog.state.insert_cluster(
                id,
                name,
                linked_object_id,
                all_indexes,
                owner_id,
                PrivilegeMap::from_mz_acl_items(privileges),
                config.into(),
            );
        }

        let replicas = catalog.storage().await.get_cluster_replicas().await?;
        for mz_catalog::ClusterReplica {
            cluster_id,
            replica_id,
            name,
            config,
            owner_id,
        } in replicas
        {
            let logging = ReplicaLogging {
                log_logging: config.logging.log_logging,
                interval: config.logging.interval,
            };
            let config = ReplicaConfig {
                location: catalog.concretize_replica_location(
                    config.location,
                    &vec![],
                    cluster_azs.get(&cluster_id).map(|zones| &**zones),
                )?,
                compute: ComputeReplicaConfig {
                    logging,
                    idle_arrangement_merge_effort: config.idle_arrangement_merge_effort,
                },
            };

            // And write the allocated sources back to storage
            catalog
                .storage()
                .await
                .set_replica_config(
                    replica_id,
                    cluster_id,
                    name.clone(),
                    config.clone().into(),
                    owner_id,
                )
                .await?;

            catalog
                .state
                .insert_cluster_replica(cluster_id, name, replica_id, config, owner_id);
        }

        for (builtin, id) in builtin_indexes {
            let schema_id = catalog.state.ambient_schemas_by_name[builtin.schema()];
            let name = QualifiedItemName {
                qualifiers: ItemQualifiers {
                    database_spec: ResolvedDatabaseSpecifier::Ambient,
                    schema_spec: SchemaSpecifier::Id(schema_id),
                },
                item: builtin.name().into(),
            };
            match builtin {
                Builtin::Index(index) => {
                    let mut item = catalog
                        .parse_item(
                            id,
                            index.sql.into(),
                            None,
                            index.is_retained_metrics_object,
                            if index.is_retained_metrics_object { Some(catalog.state.system_config().metrics_retention())} else { None },
                        )
                        .unwrap_or_else(|e| {
                            panic!(
                                "internal error: failed to load bootstrap index:\n\
                                    {}\n\
                                    error:\n\
                                    {:?}\n\n\
                                    make sure that the schema name is specified in the builtin index's create sql statement.",
                                index.name, e
                            )
                        });
                    let CatalogItem::Index(_) = &mut item else {
                        panic!("internal error: builtin index {}'s SQL does not begin with \"CREATE INDEX\".", index.name);
                    };

                    let oid = catalog.allocate_oid()?;
                    catalog.state.insert_item(
                        id,
                        oid,
                        name,
                        item,
                        MZ_SYSTEM_ROLE_ID,
                        PrivilegeMap::default(),
                    );
                }
                Builtin::Log(_)
                | Builtin::Table(_)
                | Builtin::View(_)
                | Builtin::Type(_)
                | Builtin::Func(_)
                | Builtin::Source(_) => {
                    unreachable!("handled above")
                }
            }
        }

        let new_system_id_mappings = new_builtins
            .iter()
            .map(|(builtin, id)| SystemObjectMapping {
                description: SystemObjectDescription {
                    schema_name: builtin.schema().to_string(),
                    object_type: builtin.catalog_item_type(),
                    object_name: builtin.name().to_string(),
                },
                unique_identifier: SystemObjectUniqueIdentifier {
                    id: *id,
                    fingerprint: builtin.fingerprint(),
                },
            })
            .collect();
        catalog
            .storage()
            .await
            .set_system_items(new_system_id_mappings)
            .await?;

        let last_seen_version = catalog
            .storage()
            .await
            .get_catalog_content_version()
            .await?
            .unwrap_or_else(|| "new".to_string());

        if !config.skip_migrations {
            migrate::migrate(&mut catalog, config.connection_context)
                .await
                .map_err(|e| {
                    Error::new(ErrorKind::FailedMigration {
                        last_seen_version: last_seen_version.clone(),
                        this_version: catalog.config().build_info.version,
                        cause: e.to_string(),
                    })
                })?;
            catalog
                .storage()
                .await
                .set_catalog_content_version(catalog.config().build_info.version)
                .await?;
        }

        let mut catalog = {
            let mut storage = catalog.storage().await;
            let mut tx = storage.transaction().await?;
            let catalog = Self::load_catalog_items(&mut tx, &catalog)?;
            tx.commit().await?;
            catalog
        };

        let mut builtin_migration_metadata = catalog
            .generate_builtin_migration_metadata(migrated_builtins, id_fingerprint_map)
            .await?;
        catalog.apply_in_memory_builtin_migration(&mut builtin_migration_metadata)?;
        catalog
            .apply_persisted_builtin_migration(&mut builtin_migration_metadata)
            .await?;

        // Load public keys for SSH connections from the secrets store to the catalog
        for (id, entry) in catalog.state.entry_by_id.iter_mut() {
            if let CatalogItem::Connection(ref mut connection) = entry.item {
                if let mz_storage_types::connections::Connection::Ssh(ref mut ssh) =
                    connection.connection
                {
                    let secret = config.secrets_reader.read(*id).await?;
                    let keyset = SshKeyPairSet::from_bytes(&secret)?;
                    let public_key_pair = keyset.public_keys();
                    ssh.public_keys = Some(public_key_pair);
                }
            }
        }

        let mut builtin_table_updates = vec![];
        for (schema_id, schema) in &catalog.state.ambient_schemas_by_id {
            let db_spec = ResolvedDatabaseSpecifier::Ambient;
            builtin_table_updates.push(catalog.state.pack_schema_update(&db_spec, schema_id, 1));
            for (_item_name, item_id) in &schema.items {
                builtin_table_updates.extend(catalog.state.pack_item_update(*item_id, 1));
            }
            for (_item_name, function_id) in &schema.functions {
                builtin_table_updates.extend(catalog.state.pack_item_update(*function_id, 1));
            }
        }
        for (_id, db) in &catalog.state.database_by_id {
            builtin_table_updates.push(catalog.state.pack_database_update(db, 1));
            let db_spec = ResolvedDatabaseSpecifier::Id(db.id.clone());
            for (schema_id, schema) in &db.schemas_by_id {
                builtin_table_updates
                    .push(catalog.state.pack_schema_update(&db_spec, schema_id, 1));
                for (_item_name, item_id) in &schema.items {
                    builtin_table_updates.extend(catalog.state.pack_item_update(*item_id, 1));
                }
                for (_item_name, function_id) in &schema.functions {
                    builtin_table_updates.extend(catalog.state.pack_item_update(*function_id, 1));
                }
            }
        }
        for (id, sub_component, comment) in catalog.state.comments.iter() {
            builtin_table_updates.push(catalog.state.pack_comment_update(
                id,
                sub_component,
                comment,
                1,
            ));
        }
        for (_id, role) in &catalog.state.roles_by_id {
            if let Some(builtin_update) = catalog.state.pack_role_update(role.id, 1) {
                builtin_table_updates.push(builtin_update);
            }
            for group_id in role.membership.map.keys() {
                builtin_table_updates.push(
                    catalog
                        .state
                        .pack_role_members_update(*group_id, role.id, 1),
                )
            }
        }
        for (default_privilege_object, default_privilege_acl_items) in
            catalog.state.default_privileges.iter()
        {
            for default_privilege_acl_item in default_privilege_acl_items {
                builtin_table_updates.push(catalog.state.pack_default_privileges_update(
                    default_privilege_object,
                    &default_privilege_acl_item.grantee,
                    &default_privilege_acl_item.acl_mode,
                    1,
                ));
            }
        }
        for system_privilege in catalog.state.system_privileges.all_values_owned() {
            builtin_table_updates.push(
                catalog
                    .state
                    .pack_system_privileges_update(system_privilege, 1),
            );
        }
        for (id, cluster) in &catalog.state.clusters_by_id {
            builtin_table_updates.push(catalog.state.pack_cluster_update(&cluster.name, 1));
            if let Some(linked_object_id) = cluster.linked_object_id {
                builtin_table_updates.push(catalog.state.pack_cluster_link_update(
                    &cluster.name,
                    linked_object_id,
                    1,
                ));
            }
            for (replica_name, replica_id) in cluster.replicas().map(|r| (&r.name, r.replica_id)) {
                builtin_table_updates.extend(catalog.state.pack_cluster_replica_update(
                    *id,
                    replica_name,
                    1,
                ));
                let replica = catalog.state.get_cluster_replica(*id, replica_id);
                for process_id in 0..replica.config.location.num_processes() {
                    let update = catalog.state.pack_cluster_replica_status_update(
                        *id,
                        replica_id,
                        u64::cast_from(process_id),
                        1,
                    );
                    builtin_table_updates.push(update);
                }
            }
        }
        // Operators aren't stored in the catalog, but we would like them in
        // introspection views.
        for (op, func) in OP_IMPLS.iter() {
            match func {
                mz_sql::func::Func::Scalar(impls) => {
                    for imp in impls {
                        builtin_table_updates.push(catalog.state.pack_op_update(
                            op,
                            imp.details(),
                            1,
                        ));
                    }
                }
                _ => unreachable!("all operators must be scalar functions"),
            }
        }
        let audit_logs = catalog.storage().await.get_audit_logs().await?;
        for event in audit_logs {
            builtin_table_updates.push(catalog.state.pack_audit_log_update(&event)?);
        }

        // To avoid reading over storage_usage events multiple times, do both
        // the table updates and delete calculations in a single read over the
        // data.
        let storage_usage_events = catalog
            .storage()
            .await
            .get_and_prune_storage_usage(config.storage_usage_retention_period, boot_ts)
            .await?;
        for event in storage_usage_events {
            builtin_table_updates.push(catalog.state.pack_storage_usage_update(&event)?);
        }

        for ip in &catalog.state.egress_ips {
            builtin_table_updates.push(catalog.state.pack_egress_ip_update(ip)?);
        }

        Ok((
            catalog,
            builtin_migration_metadata,
            builtin_table_updates,
            last_seen_version,
        ))
    }

    /// Loads the system configuration from the various locations in which its
    /// values and value overrides can reside.
    ///
    /// This method should _always_ be called during catalog creation _before_
    /// any other operations that depend on system configuration values.
    ///
    /// Configuration is loaded in the following order:
    ///
    /// 1. Load parameters from the configuration persisted in the catalog
    ///    storage backend.
    /// 2. Set defaults from configuration passed in the provided
    ///    `system_parameter_defaults` map.
    /// 3. Overwrite and persist selected parameter values from the
    ///    configuration that can be pulled from the provided
    ///    `system_parameter_sync_config` (if present).
    ///
    /// # Errors
    #[tracing::instrument(level = "info", skip_all)]
    async fn load_system_configuration(
        &mut self,
        system_parameter_defaults: BTreeMap<String, String>,
        system_parameter_sync_config: Option<SystemParameterSyncConfig>,
        boot_ts: mz_repr::Timestamp,
    ) -> Result<(), AdapterError> {
        let system_config = self.storage().await.get_system_configurations().await?;

        for (name, value) in &system_parameter_defaults {
            match self
                .state
                .set_system_configuration_default(name, VarInput::Flat(value))
            {
                Ok(_) => (),
                Err(AdapterError::VarError(VarError::UnknownParameter(name))) => {
                    warn!(%name, "cannot load unknown system parameter from stash");
                }
                Err(e) => return Err(e),
            };
        }
        for mz_catalog::SystemConfiguration { name, value } in system_config {
            match self
                .state
                .insert_system_configuration(&name, VarInput::Flat(&value))
            {
                Ok(_) => (),
                Err(AdapterError::VarError(VarError::UnknownParameter(name))) => {
                    warn!(%name, "cannot load unknown system parameter from stash");
                }
                Err(e) => return Err(e),
            };
        }
        if let Some(system_parameter_sync_config) = system_parameter_sync_config {
            if self.storage().await.is_read_only() {
                tracing::info!("parameter sync on boot: skipping sync as catalog is read-only");
            } else if !self.state.system_config().config_has_synced_once() {
                tracing::info!("parameter sync on boot: start sync");

                // We intentionally block initial startup, potentially forever,
                // on initializing LaunchDarkly. This may seem scary, but the
                // alternative is even scarier. Over time, we expect that the
                // compiled-in default values for the system parameters will
                // drift substantially from the defaults configured in
                // LaunchDarkly, to the point that starting an environment
                // without loading the latest values from LaunchDarkly will
                // result in running an untested configuration.
                //
                // Note this only applies during initial startup. Restarting
                // after we've synced once doesn't block on LaunchDarkly, as it
                // seems reasonable to assume that the last-synced configuration
                // was valid enough.
                //
                // This philosophy appears to provide a good balance between not
                // running untested configurations in production while also not
                // making LaunchDarkly a "tier 1" dependency for existing
                // environments.
                //
                // If this proves to be an issue, we could seek to address the
                // configuration drift in a different way--for example, by
                // writing a script that runs in CI nightly and checks for
                // deviation between the compiled Rust code and LaunchDarkly.
                //
                // If it is absolutely necessary to bring up a new environment
                // while LaunchDarkly is down, the following manual mitigation
                // can be performed:
                //
                //    1. Edit the environmentd startup parameters to omit the
                //       LaunchDarkly configuration.
                //    2. Boot environmentd.
                //    3. Run `ALTER SYSTEM config_has_synced_once = true`.
                //    4. Adjust any other parameters as necessary to avoid
                //       running a nonstandard configuration in production.
                //    5. Edit the environmentd startup parameters to restore the
                //       LaunchDarkly configuration, for when LaunchDarkly comes
                //       back online.
                //    6. Reboot environmentd.

                let mut params = SynchronizedParameters::new(self.state.system_config().clone());
                let frontend = SystemParameterFrontend::from(&system_parameter_sync_config).await?;
                frontend.pull(&mut params);
                let ops = params
                    .modified()
                    .into_iter()
                    .map(|param| {
                        let name = param.name;
                        let value = param.value;
                        tracing::debug!(name, value, "sync parameter");
                        Op::UpdateSystemConfiguration {
                            name,
                            value: OwnedVarInput::Flat(value),
                        }
                    })
                    .chain(std::iter::once({
                        let name = CONFIG_HAS_SYNCED_ONCE.name().to_string();
                        let value = true.to_string();
                        tracing::debug!(name, value, "sync parameter");
                        Op::UpdateSystemConfiguration {
                            name,
                            value: OwnedVarInput::Flat(value),
                        }
                    }))
                    .collect::<Vec<_>>();
                self.transact(boot_ts, None, ops, |_| Ok(()))
                    .await
                    .unwrap_or_terminate("cannot fail to transact");
                tracing::info!("parameter sync on boot: end sync");
            } else {
                tracing::info!("parameter sync on boot: skipping sync as config has synced once");
            }
        }
        Ok(())
    }

    /// Loads built-in system types into the catalog.
    ///
    /// Built-in types sometimes have references to other built-in types, and sometimes these
    /// references are circular. This makes loading built-in types more complicated than other
    /// built-in objects, and requires us to make multiple passes over the types to correctly
    /// resolve all references.
    #[tracing::instrument(level = "info", skip_all)]
    async fn load_builtin_types(&mut self) -> Result<(), Error> {
        let persisted_builtin_ids: BTreeMap<_, _> = self
            .storage()
            .await
            .get_system_items()
            .await?
            .into_iter()
            .map(|mapping| (mapping.description, mapping.unique_identifier))
            .collect();

        let AllocatedBuiltinSystemIds {
            all_builtins,
            new_builtins,
            migrated_builtins,
        } = self
            .allocate_system_ids(BUILTINS::types().collect(), |typ| {
                persisted_builtin_ids
                    .get(&SystemObjectDescription {
                        schema_name: typ.schema.to_string(),
                        object_type: CatalogItemType::Type,
                        object_name: typ.name.to_string(),
                    })
                    .cloned()
            })
            .await?;
        assert!(migrated_builtins.is_empty(), "types cannot be migrated");
        let name_to_id_map: BTreeMap<&str, GlobalId> = all_builtins
            .into_iter()
            .map(|(typ, id)| (typ.name, id))
            .collect();

        // Replace named references with id references
        let mut builtin_types: Vec<_> = BUILTINS::types()
            .map(|typ| Self::resolve_builtin_type(typ, &name_to_id_map))
            .collect();

        // Resolve array_id for types
        let mut element_id_to_array_id = BTreeMap::new();
        for typ in &builtin_types {
            match &typ.details.typ {
                CatalogType::Array { element_reference } => {
                    let array_id = name_to_id_map[typ.name];
                    element_id_to_array_id.insert(*element_reference, array_id);
                }
                _ => {}
            }
        }
        let pg_catalog_schema_id = self.state.get_pg_catalog_schema_id().clone();
        for typ in &mut builtin_types {
            let element_id = name_to_id_map[typ.name];
            typ.details.array_id = element_id_to_array_id.get(&element_id).map(|id| id.clone());
        }

        // Insert into catalog
        for typ in builtin_types {
            let element_id = name_to_id_map[typ.name];
            self.state.insert_item(
                element_id,
                typ.oid,
                QualifiedItemName {
                    qualifiers: ItemQualifiers {
                        database_spec: ResolvedDatabaseSpecifier::Ambient,
                        schema_spec: SchemaSpecifier::Id(pg_catalog_schema_id),
                    },
                    item: typ.name.to_owned(),
                },
                CatalogItem::Type(Type {
                    create_sql: format!("CREATE TYPE {}", typ.name),
                    details: typ.details.clone(),
                    resolved_ids: ResolvedIds(BTreeSet::new()),
                }),
                MZ_SYSTEM_ROLE_ID,
                PrivilegeMap::from_mz_acl_items(vec![
                    rbac::default_builtin_object_privilege(mz_sql::catalog::ObjectType::Type),
                    rbac::owner_privilege(mz_sql::catalog::ObjectType::Type, MZ_SYSTEM_ROLE_ID),
                ]),
            );
        }

        let new_system_id_mappings = new_builtins
            .iter()
            .map(|(typ, id)| SystemObjectMapping {
                description: SystemObjectDescription {
                    schema_name: typ.schema.to_string(),
                    object_type: CatalogItemType::Type,
                    object_name: typ.name.to_string(),
                },
                unique_identifier: SystemObjectUniqueIdentifier {
                    id: *id,
                    fingerprint: typ.fingerprint(),
                },
            })
            .collect();
        self.storage()
            .await
            .set_system_items(new_system_id_mappings)
            .await?;

        Ok(())
    }

    /// The objects in the catalog form one or more DAGs (directed acyclic graph) via object
    /// dependencies. To migrate a builtin object we must drop that object along with all of its
    /// descendants, and then recreate that object along with all of its descendants using new
    /// GlobalId`s. To achieve this we perform a DFS (depth first search) on the catalog items
    /// starting with the nodes that correspond to builtin objects that have changed schemas.
    ///
    /// Objects need to be dropped starting from the leafs of the DAG going up towards the roots,
    /// and they need to be recreated starting at the roots of the DAG and going towards the leafs.
    async fn generate_builtin_migration_metadata(
        &self,
        migrated_ids: Vec<GlobalId>,
        id_fingerprint_map: BTreeMap<GlobalId, String>,
    ) -> Result<BuiltinMigrationMetadata, Error> {
        // First obtain a topological sorting of all migrated objects and their children.
        let mut visited_set = BTreeSet::new();
        let mut topological_sort = Vec::new();
        for id in migrated_ids {
            if !visited_set.contains(&id) {
                let migrated_topological_sort = self.topological_sort(id, &mut visited_set);
                topological_sort.extend(migrated_topological_sort);
            }
        }
        topological_sort.reverse();

        // Then process all objects in sorted order.
        let mut migration_metadata = BuiltinMigrationMetadata::new();
        let mut ancestor_ids = BTreeMap::new();
        let mut migrated_log_ids = BTreeMap::new();
        let log_name_map: BTreeMap<_, _> = BUILTINS::logs()
            .map(|log| (log.variant.clone(), log.name))
            .collect();
        for entry in topological_sort {
            let id = entry.id();

            let new_id = match id {
                GlobalId::System(_) => self
                    .storage()
                    .await
                    .allocate_system_ids(1)
                    .await?
                    .into_element(),
                GlobalId::User(_) => self.storage().await.allocate_user_id().await?,
                _ => unreachable!("can't migrate id: {id}"),
            };

            let name = self.resolve_full_name(entry.name(), None);
            info!("migrating {name} from {id} to {new_id}");

            // Generate value to update fingerprint and global ID persisted mapping for system objects.
            // Not every system object has a fingerprint, like introspection source indexes.
            if let Some(fingerprint) = id_fingerprint_map.get(&id) {
                assert!(
                    id.is_system(),
                    "id_fingerprint_map should only contain builtin objects"
                );
                let schema_name = self
                    .get_schema(
                        &entry.name().qualifiers.database_spec,
                        &entry.name().qualifiers.schema_spec,
                        entry.conn_id().unwrap_or(&SYSTEM_CONN_ID),
                    )
                    .name
                    .schema
                    .as_str();
                migration_metadata.migrated_system_object_mappings.insert(
                    id,
                    SystemObjectMapping {
                        description: SystemObjectDescription {
                            schema_name: schema_name.to_string(),
                            object_type: entry.item_type(),
                            object_name: entry.name().item.clone(),
                        },
                        unique_identifier: SystemObjectUniqueIdentifier {
                            id: new_id,
                            fingerprint: fingerprint.clone(),
                        },
                    },
                );
            }

            ancestor_ids.insert(id, new_id);

            // Push drop commands.
            match entry.item() {
                CatalogItem::Table(_) | CatalogItem::Source(_) => {
                    migration_metadata.previous_source_ids.push(id)
                }
                CatalogItem::Sink(_) => migration_metadata.previous_sink_ids.push(id),
                CatalogItem::MaterializedView(_) => {
                    migration_metadata.previous_materialized_view_ids.push(id)
                }
                CatalogItem::Log(log) => {
                    migrated_log_ids.insert(id, log.variant.clone());
                }
                CatalogItem::Index(index) => {
                    if id.is_system() {
                        if let Some(variant) = migrated_log_ids.get(&index.on) {
                            migration_metadata
                                .introspection_source_index_updates
                                .entry(index.cluster_id)
                                .or_default()
                                .push((
                                    variant.clone(),
                                    log_name_map
                                        .get(variant)
                                        .expect("all variants have a name")
                                        .to_string(),
                                    new_id,
                                ));
                        }
                    }
                }
                CatalogItem::View(_) => {
                    // Views don't have any external objects to drop.
                }
                CatalogItem::Type(_)
                | CatalogItem::Func(_)
                | CatalogItem::Secret(_)
                | CatalogItem::Connection(_) => unreachable!(
                    "impossible to migrate schema for builtin {}",
                    entry.item().typ()
                ),
            }
            if id.is_user() {
                migration_metadata.user_drop_ops.push(id);
            }
            migration_metadata.all_drop_ops.push(id);

            // Push create commands.
            let name = entry.name().clone();
            if id.is_user() {
                let schema_id = name.qualifiers.schema_spec.clone().into();
                migration_metadata
                    .user_create_ops
                    .push((new_id, schema_id, name.item.clone()));
            }
            let item_rebuilder = CatalogItemRebuilder::new(entry, new_id, &ancestor_ids);
            migration_metadata.all_create_ops.push((
                new_id,
                entry.oid(),
                name,
                entry.owner_id().clone(),
                entry.privileges().clone(),
                item_rebuilder,
            ));
        }

        // Reverse drop commands.
        migration_metadata.previous_sink_ids.reverse();
        migration_metadata.previous_materialized_view_ids.reverse();
        migration_metadata.previous_source_ids.reverse();
        migration_metadata.all_drop_ops.reverse();
        migration_metadata.user_drop_ops.reverse();

        Ok(migration_metadata)
    }

    fn topological_sort(
        &self,
        id: GlobalId,
        visited_set: &mut BTreeSet<GlobalId>,
    ) -> Vec<&CatalogEntry> {
        let mut topological_sort = Vec::new();
        visited_set.insert(id);
        let entry = self.get_entry(&id);
        for dependant in entry.used_by() {
            if !visited_set.contains(dependant) {
                let child_topological_sort = self.topological_sort(*dependant, visited_set);
                topological_sort.extend(child_topological_sort);
            }
        }
        topological_sort.push(entry);
        topological_sort
    }

    fn apply_in_memory_builtin_migration(
        &mut self,
        migration_metadata: &mut BuiltinMigrationMetadata,
    ) -> Result<(), Error> {
        assert_eq!(
            migration_metadata.all_drop_ops.len(),
            migration_metadata.all_create_ops.len(),
            "we should be re-creating every dropped object"
        );
        for id in migration_metadata.all_drop_ops.drain(..) {
            self.state.drop_item(id);
            self.drop_plans_and_metainfos(id);
        }
        for (id, oid, name, owner_id, privileges, item_rebuilder) in
            migration_metadata.all_create_ops.drain(..)
        {
            let item = item_rebuilder.build(self);
            self.state
                .insert_item(id, oid, name, item, owner_id, privileges);
        }
        for (cluster_id, updates) in &migration_metadata.introspection_source_index_updates {
            let log_indexes = &mut self
                .state
                .clusters_by_id
                .get_mut(cluster_id)
                .unwrap_or_else(|| panic!("invalid cluster {cluster_id}"))
                .log_indexes;
            for (variant, _name, new_id) in updates {
                log_indexes.remove(variant);
                log_indexes.insert(variant.clone(), new_id.clone());
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "info", skip_all)]
    async fn apply_persisted_builtin_migration(
        &self,
        migration_metadata: &mut BuiltinMigrationMetadata,
    ) -> Result<(), Error> {
        let mut storage = self.storage().await;
        let mut tx = storage.transaction().await?;
        tx.remove_items(migration_metadata.user_drop_ops.drain(..).collect())?;
        for (id, schema_id, name) in migration_metadata.user_create_ops.drain(..) {
            let entry = self.get_entry(&id);
            let item = entry.item();
            let serialized_item = item.to_serialized();
            tx.insert_item(
                id,
                schema_id,
                &name,
                serialized_item,
                entry.owner_id().clone(),
                entry.privileges().all_values_owned().collect(),
            )?;
        }
        tx.update_system_object_mappings(std::mem::take(
            &mut migration_metadata.migrated_system_object_mappings,
        ))?;
        tx.update_introspection_source_index_gids(
            std::mem::take(&mut migration_metadata.introspection_source_index_updates)
                .into_iter()
                .map(|(cluster_id, updates)| {
                    (
                        cluster_id,
                        updates
                            .into_iter()
                            .map(|(_variant, name, index_id)| (name, index_id)),
                    )
                }),
        )?;

        tx.commit().await?;

        Ok(())
    }

    /// Takes a catalog which only has items in its on-disk storage ("unloaded")
    /// and cannot yet resolve names, and returns a catalog loaded with those
    /// items.
    ///
    /// This function requires transactions to support loading a catalog with
    /// the transaction's currently in-flight updates to existing catalog
    /// objects, which is necessary for at least one catalog migration.
    ///
    /// TODO(justin): it might be nice if these were two different types.
    #[tracing::instrument(level = "info", skip_all)]
    pub fn load_catalog_items<'a>(
        tx: &mut mz_catalog::Transaction<'a>,
        c: &Catalog,
    ) -> Result<Catalog, Error> {
        let mut c = c.clone();
        let mut awaiting_id_dependencies: BTreeMap<GlobalId, Vec<_>> = BTreeMap::new();
        let mut awaiting_name_dependencies: BTreeMap<String, Vec<_>> = BTreeMap::new();
        let mut items: VecDeque<_> = tx.loaded_items().into_iter().collect();
        while let Some(item) = items.pop_front() {
            let d_c = item.create_sql.clone();
            // TODO(benesch): a better way of detecting when a view has depended
            // upon a non-existent logging view. This is fine for now because
            // the only goal is to produce a nicer error message; we'll bail out
            // safely even if the error message we're sniffing out changes.
            static LOGGING_ERROR: Lazy<Regex> =
                Lazy::new(|| Regex::new("mz_catalog.[^']*").expect("valid regex"));

            let catalog_item = match c.deserialize_item(item.id, d_c) {
                Ok(item) => item,
                Err(AdapterError::Catalog(Error {
                    kind: ErrorKind::Sql(SqlCatalogError::UnknownItem(name)),
                })) if LOGGING_ERROR.is_match(&name.to_string()) => {
                    return Err(Error::new(ErrorKind::UnsatisfiableLoggingDependency {
                        depender_name: name,
                    }));
                }
                // If we were missing a dependency, wait for it to be added.
                Err(AdapterError::PlanError(plan::PlanError::InvalidId(missing_dep))) => {
                    awaiting_id_dependencies
                        .entry(missing_dep)
                        .or_default()
                        .push(item);
                    continue;
                }
                // If we were missing a dependency, wait for it to be added.
                Err(AdapterError::PlanError(plan::PlanError::Catalog(
                    SqlCatalogError::UnknownItem(missing_dep),
                ))) => {
                    match GlobalId::from_str(&missing_dep) {
                        Ok(id) => {
                            awaiting_id_dependencies.entry(id).or_default().push(item);
                        }
                        Err(_) => {
                            awaiting_name_dependencies
                                .entry(missing_dep)
                                .or_default()
                                .push(item);
                        }
                    }
                    continue;
                }
                Err(e) => {
                    let name = c.resolve_full_name(&item.name, None);
                    return Err(Error::new(ErrorKind::Corruption {
                        detail: format!("failed to deserialize item {} ({}): {}", item.id, name, e),
                    }));
                }
            };
            let oid = c.allocate_oid()?;

            // Enqueue any items waiting on this dependency.
            if let Some(dependent_items) = awaiting_id_dependencies.remove(&item.id) {
                items.extend(dependent_items);
            }
            let full_name = c.resolve_full_name(&item.name, None);
            if let Some(dependent_items) = awaiting_name_dependencies.remove(&full_name.to_string())
            {
                items.extend(dependent_items);
            }

            c.state.insert_item(
                item.id,
                oid,
                item.name,
                catalog_item,
                item.owner_id,
                PrivilegeMap::from_mz_acl_items(item.privileges),
            );
        }

        // Error on any unsatisfied dependencies.
        if let Some((missing_dep, mut dependents)) = awaiting_id_dependencies.into_iter().next() {
            let mz_catalog::Item {
                id,
                name,
                create_sql: _,
                owner_id: _,
                privileges: _,
            } = dependents.remove(0);
            let name = c.resolve_full_name(&name, None);
            return Err(Error::new(ErrorKind::Corruption {
                detail: format!(
                    "failed to deserialize item {} ({}): {}",
                    id,
                    name,
                    AdapterError::PlanError(plan::PlanError::InvalidId(missing_dep))
                ),
            }));
        }

        if let Some((missing_dep, mut dependents)) = awaiting_name_dependencies.into_iter().next() {
            let mz_catalog::Item {
                id,
                name,
                create_sql: _,
                owner_id: _,
                privileges: _,
            } = dependents.remove(0);
            let name = c.resolve_full_name(&name, None);
            return Err(Error::new(ErrorKind::Corruption {
                detail: format!(
                    "failed to deserialize item {} ({}): {}",
                    id,
                    name,
                    AdapterError::Catalog(Error {
                        kind: ErrorKind::Sql(SqlCatalogError::UnknownItem(missing_dep))
                    })
                ),
            }));
        }

        c.transient_revision = 1;
        Ok(c)
    }

    /// Allocate new system ids for any new builtin objects and looks up existing system ids for
    /// existing builtin objects
    async fn allocate_system_ids<T, F>(
        &self,
        builtins: Vec<T>,
        builtin_lookup: F,
    ) -> Result<AllocatedBuiltinSystemIds<T>, Error>
    where
        T: Copy + Fingerprint,
        F: Fn(&T) -> Option<SystemObjectUniqueIdentifier>,
    {
        let new_builtin_amount = builtins
            .iter()
            .filter(|builtin| builtin_lookup(builtin).is_none())
            .count();

        let mut global_ids = self
            .storage()
            .await
            .allocate_system_ids(
                new_builtin_amount
                    .try_into()
                    .expect("builtins should fit into u64"),
            )
            .await?
            .into_iter();

        let mut all_builtins = Vec::new();
        let mut new_builtins = Vec::new();
        let mut migrated_builtins = Vec::new();
        for builtin in &builtins {
            match builtin_lookup(builtin) {
                Some(SystemObjectUniqueIdentifier {
                    id,
                    fingerprint: old_fingerprint,
                }) => {
                    all_builtins.push((*builtin, id));
                    let new_fingerprint = builtin.fingerprint();
                    if old_fingerprint != new_fingerprint {
                        migrated_builtins.push(id);
                    }
                }
                None => {
                    let id = global_ids.next().expect("not enough global IDs");
                    all_builtins.push((*builtin, id));
                    new_builtins.push((*builtin, id));
                }
            }
        }

        Ok(AllocatedBuiltinSystemIds {
            all_builtins,
            new_builtins,
            migrated_builtins,
        })
    }
}

#[mz_ore::test(tokio::test)]
#[cfg_attr(miri, ignore)] //  unsupported operation: can't call foreign function `TLS_client_method` on OS `linux`
async fn test_builtin_migration() {
    use std::collections::{BTreeMap, BTreeSet};

    use itertools::Itertools;

    use mz_controller_types::ClusterId;
    use mz_expr::MirRelationExpr;
    use mz_ore::now::NOW_ZERO;

    use mz_repr::{GlobalId, RelationType, ScalarType};
    use mz_sql::catalog::CatalogDatabase;
    use mz_sql::names::{ItemQualifiers, QualifiedItemName, ResolvedDatabaseSpecifier};
    use mz_sql::session::user::MZ_SYSTEM_ROLE_ID;

    use crate::catalog::RelationDesc;
    use crate::catalog::{
        Catalog, CatalogItem, Index, MaterializedView, Op, OptimizedMirRelationExpr,
        DEFAULT_SCHEMA, SYSTEM_CONN_ID,
    };
    use crate::session::DEFAULT_DATABASE_NAME;

    enum ItemNamespace {
        System,
        User,
    }

    enum SimplifiedItem {
        Table,
        MaterializedView { referenced_names: Vec<String> },
        Index { on: String },
    }

    struct SimplifiedCatalogEntry {
        name: String,
        namespace: ItemNamespace,
        item: SimplifiedItem,
    }

    impl SimplifiedCatalogEntry {
        // A lot of the fields here aren't actually used in the test so we can fill them in with dummy
        // values.
        fn to_catalog_item(
            self,
            id_mapping: &BTreeMap<String, GlobalId>,
        ) -> (String, ItemNamespace, CatalogItem) {
            let item = match self.item {
                SimplifiedItem::Table => CatalogItem::Table(Table {
                    create_sql: "TODO".to_string(),
                    desc: RelationDesc::empty()
                        .with_column("a", ScalarType::Int32.nullable(true))
                        .with_key(vec![0]),
                    defaults: vec![Expr::null(); 1],
                    conn_id: None,
                    resolved_ids: ResolvedIds(BTreeSet::new()),
                    custom_logical_compaction_window: None,
                    is_retained_metrics_object: false,
                }),
                SimplifiedItem::MaterializedView { referenced_names } => {
                    let table_list = referenced_names.iter().join(",");
                    let resolved_ids = convert_name_vec_to_id_vec(referenced_names, id_mapping);
                    CatalogItem::MaterializedView(MaterializedView {
                        create_sql: format!(
                            "CREATE MATERIALIZED VIEW mv AS SELECT * FROM {table_list}"
                        ),
                        optimized_expr: OptimizedMirRelationExpr(MirRelationExpr::Constant {
                            rows: Ok(Vec::new()),
                            typ: RelationType {
                                column_types: Vec::new(),
                                keys: Vec::new(),
                            },
                        }),
                        desc: RelationDesc::empty()
                            .with_column("a", ScalarType::Int32.nullable(true))
                            .with_key(vec![0]),
                        resolved_ids: ResolvedIds(BTreeSet::from_iter(resolved_ids)),
                        cluster_id: ClusterId::User(1),
                    })
                }
                SimplifiedItem::Index { on } => {
                    let on_id = id_mapping[&on];
                    CatalogItem::Index(Index {
                        create_sql: format!("CREATE INDEX idx ON {on} (a)"),
                        on: on_id,
                        keys: Vec::new(),
                        conn_id: None,
                        resolved_ids: ResolvedIds(BTreeSet::from_iter([on_id])),
                        cluster_id: ClusterId::User(1),
                        custom_logical_compaction_window: None,
                        is_retained_metrics_object: false,
                    })
                }
            };
            (self.name, self.namespace, item)
        }
    }

    struct BuiltinMigrationTestCase {
        test_name: &'static str,
        initial_state: Vec<SimplifiedCatalogEntry>,
        migrated_names: Vec<String>,
        expected_previous_sink_names: Vec<String>,
        expected_previous_materialized_view_names: Vec<String>,
        expected_previous_source_names: Vec<String>,
        expected_all_drop_ops: Vec<String>,
        expected_user_drop_ops: Vec<String>,
        expected_all_create_ops: Vec<String>,
        expected_user_create_ops: Vec<String>,
        expected_migrated_system_object_mappings: Vec<String>,
    }

    async fn add_item(
        catalog: &mut Catalog,
        name: String,
        item: CatalogItem,
        item_namespace: ItemNamespace,
    ) -> GlobalId {
        let id = match item_namespace {
            ItemNamespace::User => catalog
                .allocate_user_id()
                .await
                .expect("cannot fail to allocate user ids"),
            ItemNamespace::System => catalog
                .allocate_system_id()
                .await
                .expect("cannot fail to allocate system ids"),
        };
        let oid = catalog
            .allocate_oid()
            .expect("cannot fail to allocate oids");
        let database_id = catalog
            .resolve_database(DEFAULT_DATABASE_NAME)
            .expect("failed to resolve default database")
            .id();
        let database_spec = ResolvedDatabaseSpecifier::Id(database_id);
        let schema_spec = catalog
            .resolve_schema_in_database(&database_spec, DEFAULT_SCHEMA, &SYSTEM_CONN_ID)
            .expect("failed to resolve default schemas")
            .id
            .clone();
        catalog
            .transact(
                mz_repr::Timestamp::MIN,
                None,
                vec![Op::CreateItem {
                    id,
                    oid,
                    name: QualifiedItemName {
                        qualifiers: ItemQualifiers {
                            database_spec,
                            schema_spec,
                        },
                        item: name,
                    },
                    item,
                    owner_id: MZ_SYSTEM_ROLE_ID,
                }],
                |_| Ok(()),
            )
            .await
            .expect("failed to transact");
        id
    }

    fn convert_name_vec_to_id_vec(
        name_vec: Vec<String>,
        id_lookup: &BTreeMap<String, GlobalId>,
    ) -> Vec<GlobalId> {
        name_vec.into_iter().map(|name| id_lookup[&name]).collect()
    }

    fn convert_id_vec_to_name_vec(
        id_vec: Vec<GlobalId>,
        name_lookup: &BTreeMap<GlobalId, String>,
    ) -> Vec<String> {
        id_vec
            .into_iter()
            .map(|id| name_lookup[&id].clone())
            .collect()
    }

    let test_cases = vec![
        BuiltinMigrationTestCase {
            test_name: "no_migrations",
            initial_state: vec![SimplifiedCatalogEntry {
                name: "s1".to_string(),
                namespace: ItemNamespace::System,
                item: SimplifiedItem::Table,
            }],
            migrated_names: vec![],
            expected_previous_sink_names: vec![],
            expected_previous_materialized_view_names: vec![],
            expected_previous_source_names: vec![],
            expected_all_drop_ops: vec![],
            expected_user_drop_ops: vec![],
            expected_all_create_ops: vec![],
            expected_user_create_ops: vec![],
            expected_migrated_system_object_mappings: vec![],
        },
        BuiltinMigrationTestCase {
            test_name: "single_migrations",
            initial_state: vec![SimplifiedCatalogEntry {
                name: "s1".to_string(),
                namespace: ItemNamespace::System,
                item: SimplifiedItem::Table,
            }],
            migrated_names: vec!["s1".to_string()],
            expected_previous_sink_names: vec![],
            expected_previous_materialized_view_names: vec![],
            expected_previous_source_names: vec!["s1".to_string()],
            expected_all_drop_ops: vec!["s1".to_string()],
            expected_user_drop_ops: vec![],
            expected_all_create_ops: vec!["s1".to_string()],
            expected_user_create_ops: vec![],
            expected_migrated_system_object_mappings: vec!["s1".to_string()],
        },
        BuiltinMigrationTestCase {
            test_name: "child_migrations",
            initial_state: vec![
                SimplifiedCatalogEntry {
                    name: "s1".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "u1".to_string(),
                    namespace: ItemNamespace::User,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s1".to_string()],
                    },
                },
            ],
            migrated_names: vec!["s1".to_string()],
            expected_previous_sink_names: vec![],
            expected_previous_materialized_view_names: vec!["u1".to_string()],
            expected_previous_source_names: vec!["s1".to_string()],
            expected_all_drop_ops: vec!["u1".to_string(), "s1".to_string()],
            expected_user_drop_ops: vec!["u1".to_string()],
            expected_all_create_ops: vec!["s1".to_string(), "u1".to_string()],
            expected_user_create_ops: vec!["u1".to_string()],
            expected_migrated_system_object_mappings: vec!["s1".to_string()],
        },
        BuiltinMigrationTestCase {
            test_name: "multi_child_migrations",
            initial_state: vec![
                SimplifiedCatalogEntry {
                    name: "s1".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "u1".to_string(),
                    namespace: ItemNamespace::User,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s1".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "u2".to_string(),
                    namespace: ItemNamespace::User,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s1".to_string()],
                    },
                },
            ],
            migrated_names: vec!["s1".to_string()],
            expected_previous_sink_names: vec![],
            expected_previous_materialized_view_names: vec!["u1".to_string(), "u2".to_string()],
            expected_previous_source_names: vec!["s1".to_string()],
            expected_all_drop_ops: vec!["u1".to_string(), "u2".to_string(), "s1".to_string()],
            expected_user_drop_ops: vec!["u1".to_string(), "u2".to_string()],
            expected_all_create_ops: vec!["s1".to_string(), "u2".to_string(), "u1".to_string()],
            expected_user_create_ops: vec!["u2".to_string(), "u1".to_string()],
            expected_migrated_system_object_mappings: vec!["s1".to_string()],
        },
        BuiltinMigrationTestCase {
            test_name: "topological_sort",
            initial_state: vec![
                SimplifiedCatalogEntry {
                    name: "s1".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "s2".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "u1".to_string(),
                    namespace: ItemNamespace::User,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s2".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "u2".to_string(),
                    namespace: ItemNamespace::User,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s1".to_string(), "u1".to_string()],
                    },
                },
            ],
            migrated_names: vec!["s1".to_string(), "s2".to_string()],
            expected_previous_sink_names: vec![],
            expected_previous_materialized_view_names: vec!["u2".to_string(), "u1".to_string()],
            expected_previous_source_names: vec!["s1".to_string(), "s2".to_string()],
            expected_all_drop_ops: vec![
                "u2".to_string(),
                "s1".to_string(),
                "u1".to_string(),
                "s2".to_string(),
            ],
            expected_user_drop_ops: vec!["u2".to_string(), "u1".to_string()],
            expected_all_create_ops: vec![
                "s2".to_string(),
                "u1".to_string(),
                "s1".to_string(),
                "u2".to_string(),
            ],
            expected_user_create_ops: vec!["u1".to_string(), "u2".to_string()],
            expected_migrated_system_object_mappings: vec!["s1".to_string(), "s2".to_string()],
        },
        BuiltinMigrationTestCase {
            test_name: "topological_sort_complex",
            initial_state: vec![
                SimplifiedCatalogEntry {
                    name: "s273".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "s322".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "s317".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "s349".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s273".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s421".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s273".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s295".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s273".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s296".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s295".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s320".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s295".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s340".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s295".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s318".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s295".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s323".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s295".to_string(), "s322".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s330".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s318".to_string(), "s317".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s321".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s318".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s315".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s296".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s354".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s296".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s327".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s296".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s339".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s296".to_string()],
                    },
                },
                SimplifiedCatalogEntry {
                    name: "s355".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::MaterializedView {
                        referenced_names: vec!["s315".to_string()],
                    },
                },
            ],
            migrated_names: vec![
                "s273".to_string(),
                "s317".to_string(),
                "s318".to_string(),
                "s320".to_string(),
                "s321".to_string(),
                "s322".to_string(),
                "s323".to_string(),
                "s330".to_string(),
                "s339".to_string(),
                "s340".to_string(),
            ],
            expected_previous_sink_names: vec![],
            expected_previous_materialized_view_names: vec![
                "s349".to_string(),
                "s421".to_string(),
                "s355".to_string(),
                "s315".to_string(),
                "s354".to_string(),
                "s327".to_string(),
                "s339".to_string(),
                "s296".to_string(),
                "s320".to_string(),
                "s340".to_string(),
                "s330".to_string(),
                "s321".to_string(),
                "s318".to_string(),
                "s323".to_string(),
                "s295".to_string(),
            ],
            expected_previous_source_names: vec![
                "s273".to_string(),
                "s317".to_string(),
                "s322".to_string(),
            ],
            expected_all_drop_ops: vec![
                "s349".to_string(),
                "s421".to_string(),
                "s355".to_string(),
                "s315".to_string(),
                "s354".to_string(),
                "s327".to_string(),
                "s339".to_string(),
                "s296".to_string(),
                "s320".to_string(),
                "s340".to_string(),
                "s330".to_string(),
                "s321".to_string(),
                "s318".to_string(),
                "s323".to_string(),
                "s295".to_string(),
                "s273".to_string(),
                "s317".to_string(),
                "s322".to_string(),
            ],
            expected_user_drop_ops: vec![],
            expected_all_create_ops: vec![
                "s322".to_string(),
                "s317".to_string(),
                "s273".to_string(),
                "s295".to_string(),
                "s323".to_string(),
                "s318".to_string(),
                "s321".to_string(),
                "s330".to_string(),
                "s340".to_string(),
                "s320".to_string(),
                "s296".to_string(),
                "s339".to_string(),
                "s327".to_string(),
                "s354".to_string(),
                "s315".to_string(),
                "s355".to_string(),
                "s421".to_string(),
                "s349".to_string(),
            ],
            expected_user_create_ops: vec![],
            expected_migrated_system_object_mappings: vec![
                "s322".to_string(),
                "s317".to_string(),
                "s273".to_string(),
                "s295".to_string(),
                "s323".to_string(),
                "s318".to_string(),
                "s321".to_string(),
                "s330".to_string(),
                "s340".to_string(),
                "s320".to_string(),
                "s296".to_string(),
                "s339".to_string(),
                "s327".to_string(),
                "s354".to_string(),
                "s315".to_string(),
                "s355".to_string(),
                "s421".to_string(),
                "s349".to_string(),
            ],
        },
        BuiltinMigrationTestCase {
            test_name: "system_child_migrations",
            initial_state: vec![
                SimplifiedCatalogEntry {
                    name: "s1".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Table,
                },
                SimplifiedCatalogEntry {
                    name: "s2".to_string(),
                    namespace: ItemNamespace::System,
                    item: SimplifiedItem::Index {
                        on: "s1".to_string(),
                    },
                },
            ],
            migrated_names: vec!["s1".to_string()],
            expected_previous_sink_names: vec![],
            expected_previous_materialized_view_names: vec![],
            expected_previous_source_names: vec!["s1".to_string()],
            expected_all_drop_ops: vec!["s2".to_string(), "s1".to_string()],
            expected_user_drop_ops: vec![],
            expected_all_create_ops: vec!["s1".to_string(), "s2".to_string()],
            expected_user_create_ops: vec![],
            expected_migrated_system_object_mappings: vec!["s1".to_string(), "s2".to_string()],
        },
    ];

    for test_case in test_cases {
        Catalog::with_debug(NOW_ZERO.clone(), |mut catalog| async move {
            let mut id_mapping = BTreeMap::new();
            let mut name_mapping = BTreeMap::new();
            for entry in test_case.initial_state {
                let (name, namespace, item) = entry.to_catalog_item(&id_mapping);
                let id = add_item(&mut catalog, name.clone(), item, namespace).await;
                id_mapping.insert(name.clone(), id);
                name_mapping.insert(id, name);
            }

            let migrated_ids = test_case
                .migrated_names
                .into_iter()
                .map(|name| id_mapping[&name])
                .collect();
            let id_fingerprint_map: BTreeMap<GlobalId, String> = id_mapping
                .iter()
                .filter(|(_name, id)| id.is_system())
                // We don't use the new fingerprint in this test, so we can just hard code it
                .map(|(_name, id)| (*id, "".to_string()))
                .collect();
            let migration_metadata = catalog
                .generate_builtin_migration_metadata(migrated_ids, id_fingerprint_map)
                .await
                .expect("failed to generate builtin migration metadata");

            assert_eq!(
                convert_id_vec_to_name_vec(migration_metadata.previous_sink_ids, &name_mapping),
                test_case.expected_previous_sink_names,
                "{} test failed with wrong previous sink ids",
                test_case.test_name
            );
            assert_eq!(
                convert_id_vec_to_name_vec(
                    migration_metadata.previous_materialized_view_ids,
                    &name_mapping
                ),
                test_case.expected_previous_materialized_view_names,
                "{} test failed with wrong previous materialized view ids",
                test_case.test_name
            );
            assert_eq!(
                convert_id_vec_to_name_vec(migration_metadata.previous_source_ids, &name_mapping),
                test_case.expected_previous_source_names,
                "{} test failed with wrong previous source ids",
                test_case.test_name
            );
            assert_eq!(
                convert_id_vec_to_name_vec(migration_metadata.all_drop_ops, &name_mapping),
                test_case.expected_all_drop_ops,
                "{} test failed with wrong all drop ops",
                test_case.test_name
            );
            assert_eq!(
                convert_id_vec_to_name_vec(migration_metadata.user_drop_ops, &name_mapping),
                test_case.expected_user_drop_ops,
                "{} test failed with wrong user drop ops",
                test_case.test_name
            );
            assert_eq!(
                migration_metadata
                    .all_create_ops
                    .into_iter()
                    .map(|(_, _, name, _, _, _)| name.item)
                    .collect::<Vec<_>>(),
                test_case.expected_all_create_ops,
                "{} test failed with wrong all create ops",
                test_case.test_name
            );
            assert_eq!(
                migration_metadata
                    .user_create_ops
                    .into_iter()
                    .map(|(_, _, name)| name)
                    .collect::<Vec<_>>(),
                test_case.expected_user_create_ops,
                "{} test failed with wrong user create ops",
                test_case.test_name
            );
            assert_eq!(
                migration_metadata
                    .migrated_system_object_mappings
                    .values()
                    .map(|mapping| mapping.description.object_name.clone())
                    .collect::<BTreeSet<_>>(),
                test_case
                    .expected_migrated_system_object_mappings
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                "{} test failed with wrong migrated system object mappings",
                test_case.test_name
            );
        })
        .await
    }
}