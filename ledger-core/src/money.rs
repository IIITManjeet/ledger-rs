use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Currency([u8; 3]);

impl Currency {
    pub fn new(s: &str) -> Result<Self, CoreError> {
        let bytes = s.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(|b| b.is_ascii_uppercase()) {
            return Err(CoreError::InvalidCurrency(s.to_owned()));
        }
        Ok(Currency([bytes[0], bytes[1], bytes[2]]))
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("Currency bytes are validated ASCII")
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Currency {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Currency::new(s)
    }
}

impl TryFrom<String> for Currency {
    type Error = CoreError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Currency::new(&s)
    }
}

impl From<Currency> for String {
    fn from(c: Currency) -> String {
        c.as_str().to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "i64", try_from = "i64")]
pub struct MinorUnit(i64);

impl MinorUnit {
    pub fn new(v: i64) -> Result<Self, CoreError> {
        if v <= 0 {
            return Err(CoreError::NonPositiveAmount(v));
        }
        Ok(MinorUnit(v))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for MinorUnit {
    type Error = CoreError;
    fn try_from(v: i64) -> Result<Self, Self::Error> {
        MinorUnit::new(v)
    }
}

impl From<MinorUnit> for i64 {
    fn from(m: MinorUnit) -> i64 {
        m.0
    }
}

impl fmt::Display for MinorUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn currency_accepts_three_uppercase_letters() {
        assert_eq!(Currency::new("USD").unwrap().as_str(), "USD");
        assert_eq!(Currency::new("EUR").unwrap().as_str(), "EUR");
        assert_eq!(Currency::new("JPY").unwrap().as_str(), "JPY");
    }

    #[test]
    fn currency_rejects_invalid() {
        for bad in &["", "us", "USDX", "usd", "US1", "US ", "💵💵💵"] {
            assert!(
                Currency::new(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn currency_serde_round_trip() {
        let c = Currency::new("EUR").unwrap();
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#""EUR""#);
        let back: Currency = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn currency_serde_rejects_invalid() {
        assert!(serde_json::from_str::<Currency>(r#""usd""#).is_err());
        assert!(serde_json::from_str::<Currency>(r#""US""#).is_err());
    }

    #[test]
    fn minor_unit_accepts_positive() {
        assert_eq!(MinorUnit::new(1).unwrap().get(), 1);
        assert_eq!(MinorUnit::new(i64::MAX).unwrap().get(), i64::MAX);
    }

    #[test]
    fn minor_unit_rejects_zero_and_negative() {
        assert!(matches!(
            MinorUnit::new(0),
            Err(CoreError::NonPositiveAmount(0))
        ));
        assert!(matches!(
            MinorUnit::new(-1),
            Err(CoreError::NonPositiveAmount(-1))
        ));
    }

    #[test]
    fn minor_unit_serde_round_trip() {
        let m = MinorUnit::new(12345).unwrap();
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "12345");
        let back: MinorUnit = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn minor_unit_serde_rejects_non_positive() {
        assert!(serde_json::from_str::<MinorUnit>("0").is_err());
        assert!(serde_json::from_str::<MinorUnit>("-5").is_err());
    }
}
