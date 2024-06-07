# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

import argparse
import os
import sys

"""
We invoke this script (via Bazel) and provide paths to the volatile and stable
variable files that this scripts parses and then generates a Rust file with
all the variables as static values.

See <https://bazel.build/docs/user-manual#workspace-status> for more info on
what these files are.
"""


def main():
    parser = argparse.ArgumentParser(
        description="Generate a Rust module with build-info"
    )
    parser.add_argument(
        "--rust_file",
        required=True,
        help="path of the Rust file this script will generate",
    )
    parser.add_argument(
        "--volatile_file",
        required=True,
        help="file generated by Bazel that includes the 'volatile' variables",
    )
    parser.add_argument(
        "--stable_file",
        required=True,
        help="file generated by Bazel that includes the 'stable' variables",
    )

    args = parser.parse_args()

    volatile_variables = parse_variable_file(args.volatile_file)
    stable_variables = parse_variable_file(args.stable_file)

    # Make sure the parent directory of the destination exists.
    output_dir = os.path.dirname(args.rust_file)
    if not os.path.exists(output_dir):
        os.makedirs(output_dir)

    with open(args.rust_file, "w") as f:
        for k, v in volatile_variables.items():
            key_name = k.upper()
            f.write(f'pub static {key_name}: &str = "{v}";\n')
        for k, v in stable_variables.items():
            key_name = k.upper()
            f.write(f'pub static {key_name}: &str = "{v}";\n')


def parse_variable_file(path) -> dict[str, str]:
    variables = {}

    with open(path) as f:
        for line in f.read().splitlines(keepends=False):
            if not line:
                continue

            # Note: The key value format of this Bazel generated file is that
            # the first space is what splits keys from their value.
            key_value = line.split(" ", 1)

            key = key_value[0].strip()

            if key in variables:
                sys.stderr.write(f"Error: Found duplicate key '{key}'\n")
                sys.exit(1)
            if len(key_value) == 1:
                sys.stderr.write(f"Error: No value for key '{key}'\n")
                sys.exit(1)

            variables[key] = key_value[1].strip()

    return variables


if __name__ == "__main__":
    main()