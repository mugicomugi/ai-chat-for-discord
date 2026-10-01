//! Discord IDs (snowflakes) as they arrive from operators, the web API and Discord's JSON.

/// Discord IDs travel as JSON strings: JavaScript numbers lose precision above 2^53.
pub mod id_string {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(id: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(id)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        super::parse_snowflake(&value).ok_or_else(|| D::Error::custom("invalid Discord ID"))
    }
}

/// A nonzero Discord ID written in plain decimal digits (no sign, spaces or exponent).
pub fn parse_snowflake(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|id| *id != 0)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Guild {
        #[serde(with = "id_string")]
        id: u64,
        name: String,
    }

    #[test]
    fn snowflakes_round_trip_as_strings() {
        assert_eq!(parse_snowflake("18446744073709551615"), Some(u64::MAX));
        for value in [
            "",
            "0",
            "-1",
            "+1",
            " 1",
            "1e3",
            "18446744073709551616",
            "１",
        ] {
            assert_eq!(parse_snowflake(value), None, "{value}");
        }
        let guild = Guild {
            id: 1_234_567_890_123_456_789,
            name: "サーバー".into(),
        };
        let json = serde_json::to_string(&guild).unwrap();
        assert_eq!(json, r#"{"id":"1234567890123456789","name":"サーバー"}"#);
        assert_eq!(serde_json::from_str::<Guild>(&json).unwrap(), guild);
        assert!(serde_json::from_str::<Guild>(r#"{"id":12,"name":"x"}"#).is_err());
    }
}
