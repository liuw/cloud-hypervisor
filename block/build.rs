// Copyright © 2026, Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

fn main() {
    println!("cargo::rustc-check-cfg=cfg(fuzzing)");
}
