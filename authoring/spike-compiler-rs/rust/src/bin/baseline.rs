// SPDX-License-Identifier: Apache-2.0
//! A binary with the same profile and no parser, for the size comparison.
fn main() {
    for path in std::env::args().skip(1) {
        let source = std::fs::read_to_string(&path).expect("read source");
        println!("{}", source.len());
    }
}
