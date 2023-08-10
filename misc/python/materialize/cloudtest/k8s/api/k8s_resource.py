# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

import subprocess
from textwrap import dedent
from typing import Optional

from kubernetes.client import AppsV1Api, CoreV1Api, RbacAuthorizationV1Api
from kubernetes.config import new_client_from_config  # type: ignore

from materialize import ROOT, mzbuild, ui
from materialize.cloudtest import DEFAULT_K8S_CONTEXT_NAME
from materialize.cloudtest.util.wait import wait


class K8sResource:
    def __init__(self, namespace: str):
        self.selected_namespace = namespace

    def kubectl(self, *args: str, input: Optional[str] = None) -> str:
        try:
            cmd = [
                "kubectl",
                "--context",
                self.context(),
                "--namespace",
                self.namespace(),
                *args,
            ]

            return subprocess.check_output(cmd, text=True, input=input)
        except subprocess.CalledProcessError as e:
            print(
                dedent(
                    f"""
                    cmd: {e.cmd}
                    returncode: {e.returncode}
                    stdout: {e.stdout}
                    stderr: {e.stderr}
                    """
                )
            )
            raise e

    def api(self) -> CoreV1Api:
        api_client = new_client_from_config(context=self.context())
        return CoreV1Api(api_client)

    def apps_api(self) -> AppsV1Api:
        api_client = new_client_from_config(context=self.context())
        return AppsV1Api(api_client)

    def rbac_api(self) -> RbacAuthorizationV1Api:
        api_client = new_client_from_config(context=self.context())
        return RbacAuthorizationV1Api(api_client)

    def context(self) -> str:
        return DEFAULT_K8S_CONTEXT_NAME

    def namespace(self) -> str:
        return self.selected_namespace

    def kind(self) -> str:
        assert False

    def create(self) -> None:
        assert False

    def image(
        self, service: str, tag: Optional[str] = None, release_mode: bool = True
    ) -> str:
        if tag is not None:
            return f"materialize/{service}:{tag}"
        else:
            coverage = ui.env_is_truthy("CI_COVERAGE_ENABLED")
            repo = mzbuild.Repository(
                ROOT, release_mode=release_mode, coverage=coverage
            )
            deps = repo.resolve_dependencies([repo.images[service]])
            rimage = deps[service]
            return rimage.spec()

    def wait(
        self,
        condition: str,
        resource: str,
    ) -> None:
        wait(condition=condition, resource=resource, namespace=self.selected_namespace)