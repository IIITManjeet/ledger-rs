use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn canonicalize(v: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    write_canonical(&mut out, v);
    out
}

pub fn sha256_of_body(body: &[u8]) -> Option<[u8; 32]> {
    let v: Value = serde_json::from_slice(body).ok()?;
    Some(sha256_of_value(&v))
}

pub fn sha256_of_value(v: &Value) -> [u8; 32] {
    let canonical = canonicalize(v);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    hasher.finalize().into()
}

fn write_canonical(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => {
            let escaped = serde_json::to_string(s).expect("string serialization is infallible");
            out.extend_from_slice(escaped.as_bytes());
        }
        Value::Array(arr) => {
            out.push(b'[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(out, item);
            }
            out.push(b']');
        }
        Value::Object(obj) => {
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort_unstable();
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                let escaped = serde_json::to_string(k).expect("string serialization is infallible");
                out.extend_from_slice(escaped.as_bytes());
                out.push(b':');
                write_canonical(out, &obj[*k]);
            }
            out.push(b'}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn primitives() {
        assert_eq!(canonicalize(&json!(null)), b"null");
        assert_eq!(canonicalize(&json!(true)), b"true");
        assert_eq!(canonicalize(&json!(false)), b"false");
        assert_eq!(canonicalize(&json!(0)), b"0");
        assert_eq!(canonicalize(&json!(-42)), b"-42");
        assert_eq!(canonicalize(&json!("hello")), b"\"hello\"");
        assert_eq!(canonicalize(&json!("")), b"\"\"");
    }

    #[test]
    fn strings_escape_specials() {
        assert_eq!(canonicalize(&json!("a\"b")), b"\"a\\\"b\"");
        assert_eq!(canonicalize(&json!("a\nb")), b"\"a\\nb\"");
    }

    #[test]
    fn array_compact() {
        assert_eq!(canonicalize(&json!([1, 2, 3])), b"[1,2,3]");
        assert_eq!(canonicalize(&json!([])), b"[]");
    }

    #[test]
    fn object_keys_sorted() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":2,"c":3}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"c":3,"a":2,"b":1}"#).unwrap();
        assert_eq!(canonicalize(&a), canonicalize(&b));
        assert_eq!(canonicalize(&a), br#"{"a":2,"b":1,"c":3}"#.to_vec());
    }

    #[test]
    fn nested_object_keys_sorted_recursively() {
        let a: Value =
            serde_json::from_str(r#"{"outer":{"z":1,"y":2},"first":[{"d":4,"a":1}]}"#).unwrap();
        let b: Value =
            serde_json::from_str(r#"{"first":[{"a":1,"d":4}],"outer":{"y":2,"z":1}}"#).unwrap();
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn whitespace_in_input_does_not_affect_canonical_output() {
        let a: Value = serde_json::from_str(r#"{ "a" : 1 , "b" : [ 2 , 3 ] }"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":1,"b":[2,3]}"#).unwrap();
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn sha256_is_stable_under_key_reorder() {
        let a = r#"{"b":1,"a":2,"c":3}"#.as_bytes();
        let b = r#"{"c":3,"a":2,"b":1}"#.as_bytes();
        assert_eq!(sha256_of_body(a), sha256_of_body(b));
    }

    #[test]
    fn sha256_differs_on_value_change() {
        let a = r#"{"a":1}"#.as_bytes();
        let b = r#"{"a":2}"#.as_bytes();
        assert_ne!(sha256_of_body(a), sha256_of_body(b));
    }

    fn arbitrary_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| Value::Number(n.into())),
            ".{0,16}".prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::vec(("[a-z]{1,6}", inner), 0..4).prop_map(|kvs| {
                    let mut obj = serde_json::Map::new();
                    for (k, v) in kvs {
                        obj.insert(k, v);
                    }
                    Value::Object(obj)
                }),
            ]
        })
    }

    proptest! {
        #[test]
        fn canonical_output_is_valid_json(v in arbitrary_value()) {
            let bytes = canonicalize(&v);
            let s = std::str::from_utf8(&bytes).expect("canonical output is UTF-8");
            let _: Value = serde_json::from_str(s).expect("canonical output is valid JSON");
        }

        #[test]
        fn canonical_is_idempotent(v in arbitrary_value()) {
            let bytes1 = canonicalize(&v);
            let s = std::str::from_utf8(&bytes1).unwrap();
            let v2: Value = serde_json::from_str(s).unwrap();
            let bytes2 = canonicalize(&v2);
            prop_assert_eq!(bytes1, bytes2);
        }
    }
}
