use regex::Regex;

pub fn validate_database_name(value: &str) -> anyhow::Result<()> {
    let regex = Regex::new(r"^[a-z][a-z0-9_]{2,62}$")?;
    if regex.is_match(value) {
        Ok(())
    } else {
        anyhow::bail!("Database name must match ^[a-z][a-z0-9_]{{2,62}}$ and start with a letter")
    }
}

pub fn normalize_application_name(database_name: &str, application_name: &str) -> String {
    if application_name.trim().is_empty() {
        database_name.to_owned()
    } else {
        application_name.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_application_name, validate_database_name};

    #[test]
    fn accepts_valid_database_name() {
        assert!(validate_database_name("billing_core").is_ok());
    }

    #[test]
    fn rejects_invalid_database_name() {
        assert!(validate_database_name("1billing").is_err());
    }

    #[test]
    fn defaults_application_name() {
        assert_eq!(normalize_application_name("orders", "   "), "orders");
    }
}
