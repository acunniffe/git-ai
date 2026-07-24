//! Position-encoded array abstraction.
//! Converts plain Rust structs to/from sparse JSON objects.
//!
//! Uses `Option<Option<T>>` semantics for the three states:
//! - `None` = not-set (key omitted from sparse object)
//! - `Some(None)` = null (explicit null value)
//! - `Some(Some(v))` = value

use super::types::SparseArray;
use serde_json::Value;

/// Field type for position-encoded values.
/// - `None` = not-set (key omitted from sparse object)
/// - `Some(None)` = null (explicit null value)
/// - `Some(Some(v))` = value
pub type PosField<T> = Option<Option<T>>;

/// Trait for types that can be position-encoded.
pub trait PosEncoded: Sized + Default {
    fn to_sparse(&self) -> SparseArray;
    #[allow(dead_code)]
    fn from_sparse(arr: &SparseArray) -> Self;
}

/// Convert a `PosField<String>` to JSON Value for sparse array.
/// Returns None for not-set (key should be omitted).
pub fn string_to_json(field: &PosField<String>) -> Option<Value> {
    match field {
        None => None,
        Some(None) => Some(Value::Null),
        Some(Some(s)) => Some(Value::String(s.clone())),
    }
}

/// Convert a `PosField<u32>` to JSON Value for sparse array.
pub fn u32_to_json(field: &PosField<u32>) -> Option<Value> {
    match field {
        None => None,
        Some(None) => Some(Value::Null),
        Some(Some(n)) => Some(Value::Number((*n).into())),
    }
}

/// Convert a `PosField<u64>` to JSON Value for sparse array.
pub fn u64_to_json(field: &PosField<u64>) -> Option<Value> {
    match field {
        None => None,
        Some(None) => Some(Value::Null),
        Some(Some(n)) => Some(Value::Number((*n).into())),
    }
}

/// Convert a `PosField<f64>` to JSON Value for sparse array.
pub fn f64_to_json(field: &PosField<f64>) -> Option<Value> {
    match field {
        None => None,
        Some(None) => Some(Value::Null),
        Some(Some(f)) => serde_json::Number::from_f64(*f).map(Value::Number),
    }
}

/// Get a string field from a sparse array at a position.
#[allow(dead_code)]
pub fn sparse_get_string(arr: &SparseArray, pos: usize) -> PosField<String> {
    match arr.get(&pos.to_string()) {
        None => None,                    // not-set
        Some(Value::Null) => Some(None), // explicit null
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => None, // wrong type, treat as not-set
    }
}

/// Get a u32 field from a sparse array at a position.
#[allow(dead_code)]
pub fn sparse_get_u32(arr: &SparseArray, pos: usize) -> PosField<u32> {
    match arr.get(&pos.to_string()) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Number(n)) => n.as_u64().and_then(|v| {
            if v <= u32::MAX as u64 {
                Some(Some(v as u32))
            } else {
                None
            }
        }),
        Some(_) => None,
    }
}

/// Get a u64 field from a sparse array at a position.
#[allow(dead_code)]
pub fn sparse_get_u64(arr: &SparseArray, pos: usize) -> PosField<u64> {
    match arr.get(&pos.to_string()) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Number(n)) => n.as_u64().map(Some),
        Some(_) => None,
    }
}

/// Get a f64 field from a sparse array at a position.
#[allow(dead_code)]
pub fn sparse_get_f64(arr: &SparseArray, pos: usize) -> PosField<f64> {
    match arr.get(&pos.to_string()) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Number(n)) => n.as_f64().map(Some),
        Some(_) => None,
    }
}

/// Convert a `PosField<Vec<String>>` to JSON array.
pub fn vec_string_to_json(field: &PosField<Vec<String>>) -> Option<Value> {
    match field {
        None => None,
        Some(None) => Some(Value::Null),
        Some(Some(vec)) => Some(Value::Array(
            vec.iter().map(|s| Value::String(s.clone())).collect(),
        )),
    }
}

/// Convert a `PosField<Vec<u32>>` to JSON array.
pub fn vec_u32_to_json(field: &PosField<Vec<u32>>) -> Option<Value> {
    match field {
        None => None,
        Some(None) => Some(Value::Null),
        Some(Some(vec)) => Some(Value::Array(
            vec.iter().map(|n| Value::Number((*n).into())).collect(),
        )),
    }
}

/// Convert a `PosField<Vec<u64>>` to JSON array.
pub fn vec_u64_to_json(field: &PosField<Vec<u64>>) -> Option<Value> {
    match field {
        None => None,
        Some(None) => Some(Value::Null),
        Some(Some(vec)) => Some(Value::Array(
            vec.iter().map(|n| Value::Number((*n).into())).collect(),
        )),
    }
}

/// Get a `Vec<String>` field from a sparse array at a position.
#[allow(dead_code)]
pub fn sparse_get_vec_string(arr: &SparseArray, pos: usize) -> PosField<Vec<String>> {
    match arr.get(&pos.to_string()) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Array(arr)) => {
            let strings: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            Some(Some(strings))
        }
        Some(_) => None,
    }
}

/// Get a `Vec<u32>` field from a sparse array at a position.
#[allow(dead_code)]
pub fn sparse_get_vec_u32(arr: &SparseArray, pos: usize) -> PosField<Vec<u32>> {
    match arr.get(&pos.to_string()) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Array(arr)) => {
            let nums: Vec<u32> = arr
                .iter()
                .filter_map(|v| {
                    v.as_u64().and_then(|n| {
                        if n <= u32::MAX as u64 {
                            Some(n as u32)
                        } else {
                            None
                        }
                    })
                })
                .collect();
            Some(Some(nums))
        }
        Some(_) => None,
    }
}

/// Get a `Vec<u64>` field from a sparse array at a position.
#[allow(dead_code)]
pub fn sparse_get_vec_u64(arr: &SparseArray, pos: usize) -> PosField<Vec<u64>> {
    match arr.get(&pos.to_string()) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Array(arr)) => {
            let nums: Vec<u64> = arr.iter().filter_map(|v| v.as_u64()).collect();
            Some(Some(nums))
        }
        Some(_) => None,
    }
}

/// Set a value in a sparse array at a position.
/// If value is Some, inserts; if None, does nothing (not-set).
pub fn sparse_set(arr: &mut SparseArray, pos: usize, value: Option<Value>) {
    if let Some(v) = value {
        arr.insert(pos.to_string(), v);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_to_json() {
        assert_eq!(string_to_json(&None), None);
        assert_eq!(string_to_json(&Some(None)), Some(Value::Null));
        assert_eq!(
            string_to_json(&Some(Some("test".to_string()))),
            Some(Value::String("test".to_string()))
        );
    }

    #[test]
    fn test_u32_to_json() {
        assert_eq!(u32_to_json(&None), None);
        assert_eq!(u32_to_json(&Some(None)), Some(Value::Null));
        assert_eq!(u32_to_json(&Some(Some(42))), Some(Value::Number(42.into())));
    }

    #[test]
    fn test_sparse_get_string() {
        let mut arr = SparseArray::new();
        assert_eq!(sparse_get_string(&arr, 0), None);

        arr.insert("0".to_string(), Value::Null);
        assert_eq!(sparse_get_string(&arr, 0), Some(None));

        arr.insert("1".to_string(), Value::String("test".to_string()));
        assert_eq!(sparse_get_string(&arr, 1), Some(Some("test".to_string())));
    }

    #[test]
    fn test_sparse_get_u32() {
        let mut arr = SparseArray::new();
        assert_eq!(sparse_get_u32(&arr, 0), None);

        arr.insert("0".to_string(), Value::Null);
        assert_eq!(sparse_get_u32(&arr, 0), Some(None));

        arr.insert("1".to_string(), Value::Number(42.into()));
        assert_eq!(sparse_get_u32(&arr, 1), Some(Some(42)));
    }

    #[test]
    fn test_u64_to_json() {
        assert_eq!(u64_to_json(&None), None);
        assert_eq!(u64_to_json(&Some(None)), Some(Value::Null));
        assert_eq!(
            u64_to_json(&Some(Some(12345678901234))),
            Some(Value::Number(12345678901234u64.into()))
        );
    }

    #[test]
    fn test_sparse_get_u64() {
        let mut arr = SparseArray::new();
        assert_eq!(sparse_get_u64(&arr, 0), None);

        arr.insert("0".to_string(), Value::Null);
        assert_eq!(sparse_get_u64(&arr, 0), Some(None));

        arr.insert("1".to_string(), Value::Number(12345678901234u64.into()));
        assert_eq!(sparse_get_u64(&arr, 1), Some(Some(12345678901234)));

        // Wrong type
        arr.insert("2".to_string(), Value::String("not a number".to_string()));
        assert_eq!(sparse_get_u64(&arr, 2), None);
    }

    #[test]
    fn test_vec_string_to_json() {
        assert_eq!(vec_string_to_json(&None), None);
        assert_eq!(vec_string_to_json(&Some(None)), Some(Value::Null));
        assert_eq!(
            vec_string_to_json(&Some(Some(vec!["a".to_string(), "b".to_string()]))),
            Some(Value::Array(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string())
            ]))
        );
    }

    #[test]
    fn test_vec_u32_to_json() {
        assert_eq!(vec_u32_to_json(&None), None);
        assert_eq!(vec_u32_to_json(&Some(None)), Some(Value::Null));
        assert_eq!(
            vec_u32_to_json(&Some(Some(vec![10, 20, 30]))),
            Some(Value::Array(vec![
                Value::Number(10.into()),
                Value::Number(20.into()),
                Value::Number(30.into())
            ]))
        );
    }

    #[test]
    fn test_vec_u64_to_json() {
        assert_eq!(vec_u64_to_json(&None), None);
        assert_eq!(vec_u64_to_json(&Some(None)), Some(Value::Null));
        assert_eq!(
            vec_u64_to_json(&Some(Some(vec![1000000000000u64, 2000000000000u64]))),
            Some(Value::Array(vec![
                Value::Number(1000000000000u64.into()),
                Value::Number(2000000000000u64.into())
            ]))
        );
    }

    #[test]
    fn test_sparse_get_vec_string() {
        let mut arr = SparseArray::new();
        assert_eq!(sparse_get_vec_string(&arr, 0), None);

        arr.insert("0".to_string(), Value::Null);
        assert_eq!(sparse_get_vec_string(&arr, 0), Some(None));

        arr.insert(
            "1".to_string(),
            Value::Array(vec![
                Value::String("x".to_string()),
                Value::String("y".to_string()),
            ]),
        );
        assert_eq!(
            sparse_get_vec_string(&arr, 1),
            Some(Some(vec!["x".to_string(), "y".to_string()]))
        );

        // Mixed types - filters out non-strings
        arr.insert(
            "2".to_string(),
            Value::Array(vec![
                Value::String("a".to_string()),
                Value::Number(123.into()),
                Value::String("b".to_string()),
            ]),
        );
        assert_eq!(
            sparse_get_vec_string(&arr, 2),
            Some(Some(vec!["a".to_string(), "b".to_string()]))
        );
    }

    #[test]
    fn test_sparse_get_vec_u32() {
        let mut arr = SparseArray::new();
        assert_eq!(sparse_get_vec_u32(&arr, 0), None);

        arr.insert("0".to_string(), Value::Null);
        assert_eq!(sparse_get_vec_u32(&arr, 0), Some(None));

        arr.insert(
            "1".to_string(),
            Value::Array(vec![Value::Number(10.into()), Value::Number(20.into())]),
        );
        assert_eq!(sparse_get_vec_u32(&arr, 1), Some(Some(vec![10, 20])));

        // Value too large for u32
        arr.insert(
            "2".to_string(),
            Value::Array(vec![
                Value::Number(10.into()),
                Value::Number(5000000000u64.into()),
            ]),
        );
        assert_eq!(sparse_get_vec_u32(&arr, 2), Some(Some(vec![10]))); // filters out too-large value
    }

    #[test]
    fn test_sparse_get_vec_u64() {
        let mut arr = SparseArray::new();
        assert_eq!(sparse_get_vec_u64(&arr, 0), None);

        arr.insert("0".to_string(), Value::Null);
        assert_eq!(sparse_get_vec_u64(&arr, 0), Some(None));

        arr.insert(
            "1".to_string(),
            Value::Array(vec![
                Value::Number(1000000000000u64.into()),
                Value::Number(2000000000000u64.into()),
            ]),
        );
        assert_eq!(
            sparse_get_vec_u64(&arr, 1),
            Some(Some(vec![1000000000000u64, 2000000000000u64]))
        );
    }

    #[test]
    fn test_sparse_set() {
        let mut arr = SparseArray::new();

        // Set with Some value
        sparse_set(&mut arr, 0, Some(Value::String("test".to_string())));
        assert_eq!(arr.get("0"), Some(&Value::String("test".to_string())));

        // Set with None (no-op)
        sparse_set(&mut arr, 1, None);
        assert_eq!(arr.get("1"), None);

        // Set with null value
        sparse_set(&mut arr, 2, Some(Value::Null));
        assert_eq!(arr.get("2"), Some(&Value::Null));
    }

    #[test]
    fn test_sparse_get_string_wrong_type() {
        let mut arr = SparseArray::new();
        arr.insert("0".to_string(), Value::Number(123.into()));
        // Wrong type should return None (not-set)
        assert_eq!(sparse_get_string(&arr, 0), None);
    }

    #[test]
    fn test_sparse_get_u32_wrong_type() {
        let mut arr = SparseArray::new();
        arr.insert("0".to_string(), Value::String("not a number".to_string()));
        // Wrong type should return None
        assert_eq!(sparse_get_u32(&arr, 0), None);
    }

    #[test]
    fn test_sparse_get_u32_overflow() {
        let mut arr = SparseArray::new();
        // Value larger than u32::MAX
        arr.insert("0".to_string(), Value::Number(5000000000u64.into()));
        // Should return None for overflow
        assert_eq!(sparse_get_u32(&arr, 0), None);
    }

    #[test]
    fn test_sparse_get_vec_wrong_types() {
        let mut arr = SparseArray::new();

        // Not an array
        arr.insert("0".to_string(), Value::String("not an array".to_string()));
        assert_eq!(sparse_get_vec_string(&arr, 0), None);
        assert_eq!(sparse_get_vec_u32(&arr, 0), None);
        assert_eq!(sparse_get_vec_u64(&arr, 0), None);
    }
}
