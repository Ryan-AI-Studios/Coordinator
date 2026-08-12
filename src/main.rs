//! Coordinator — product binary (bootstrap stub).
//!
//! Control-plane behavior arrives in later tracks; this crate only needs to
//! build, format, clippy-clean, and test.

fn main() {
    println!("{}", product_name());
}

fn product_name() -> &'static str {
    "coordinator"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_name_is_coordinator() {
        assert_eq!(product_name(), "coordinator");
    }
}
