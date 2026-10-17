//! Cargo race fixture: prints the line the harness expects
//! (`RACE.output` in diskgc-e2e/src/cargo_under_test.rs), so a test can
//! tell whether the binary ran.

fn main() {
    println!("diskgc-e2e cargo race fixture ran");
}

#[cfg(test)]
mod tests {
    #[test]
    fn runs() {
        println!("diskgc-e2e cargo race fixture test ran");
    }
}
