//! Field-table macro for position-encoded metrics events with heterogeneous
//! scalar/array fields.
//!
//! Complements `raw_json_event!` (single JSON-blob payloads): this macro is
//! for events made of several independently-nullable scalar/array fields,
//! each living at its own sparse-array position. Backs `CommittedValues`
//! (Event ID 1), `CheckpointValues` (Event ID 4), and `RewriteCommittedValues`
//! (Event ID 7).
//!
//! Each invocation lists the event's fields as `name: type @ POSITION_CONST`.
//! `POSITION_CONST` names a constant in a hand-written `*_pos` module --
//! deliberately kept separate from this macro, because those modules carry
//! reserved/removed position slots and (for `rewrite_committed_pos`) a
//! cross-event alignment note that don't correspond to any live field, and
//! forcing that irregular shape through the macro would contort it for no
//! DRY benefit.
//!
//! Supported field types: `u32`, `u64`, `String`, `[String]` (-> `Vec<String>`),
//! `[u32]` (-> `Vec<u32>`). The bracket form denotes the parallel-array
//! fields; add an arm to each dispatch macro below to support a new type.

/// Resolves a field's type tag to its stored `PosField<_>` inner type.
macro_rules! pos_event_storage_ty {
    (u32) => {
        u32
    };
    (u64) => {
        u64
    };
    (String) => {
        String
    };
    ([String]) => {
        Vec<String>
    };
    ([u32]) => {
        Vec<u32>
    };
}
pub(crate) use pos_event_storage_ty;

/// Resolves a field's type tag to its builder method's argument type.
///
/// String fields accept `impl Into<String>` for call-site ergonomics
/// (matches the original hand-written builders). Every other type is taken
/// by its exact stored type rather than a generic `impl Into<T>`: numeric
/// and `Vec` literals at call sites (e.g. `.human_additions(50)`,
/// `.ai_additions(vec![100, 70])`) rely on the parameter being a concrete
/// type for integer-literal inference -- a generic bound makes `50` and
/// `vec![100, 70]` ambiguous (which integer-widening `Into` impl applies?)
/// and breaks existing call sites.
macro_rules! pos_event_arg_ty {
    (u32) => {
        u32
    };
    (u64) => {
        u64
    };
    (String) => {
        impl Into<String>
    };
    ([String]) => {
        Vec<String>
    };
    ([u32]) => {
        Vec<u32>
    };
}
pub(crate) use pos_event_arg_ty;

/// Resolves a field's type tag to its `<ty>_to_json` call.
macro_rules! pos_event_to_json {
    (u32, $field:expr) => {
        $crate::model::metrics::pos_encoded::u32_to_json($field)
    };
    (u64, $field:expr) => {
        $crate::model::metrics::pos_encoded::u64_to_json($field)
    };
    (String, $field:expr) => {
        $crate::model::metrics::pos_encoded::string_to_json($field)
    };
    ([String], $field:expr) => {
        $crate::model::metrics::pos_encoded::vec_string_to_json($field)
    };
    ([u32], $field:expr) => {
        $crate::model::metrics::pos_encoded::vec_u32_to_json($field)
    };
}
pub(crate) use pos_event_to_json;

/// Resolves a field's type tag to its `sparse_get_<ty>` call.
macro_rules! pos_event_from_sparse {
    (u32, $arr:expr, $pos:expr) => {
        $crate::model::metrics::pos_encoded::sparse_get_u32($arr, $pos)
    };
    (u64, $arr:expr, $pos:expr) => {
        $crate::model::metrics::pos_encoded::sparse_get_u64($arr, $pos)
    };
    (String, $arr:expr, $pos:expr) => {
        $crate::model::metrics::pos_encoded::sparse_get_string($arr, $pos)
    };
    ([String], $arr:expr, $pos:expr) => {
        $crate::model::metrics::pos_encoded::sparse_get_vec_string($arr, $pos)
    };
    ([u32], $arr:expr, $pos:expr) => {
        $crate::model::metrics::pos_encoded::sparse_get_vec_u32($arr, $pos)
    };
}
pub(crate) use pos_event_from_sparse;

/// Defines a position-encoded metrics event values struct from a field
/// table.
///
/// Generates: the struct (`PosField<_>` per field), `new()`, per-field
/// builder + `_null` builder methods, and the `PosEncoded` + `EventValues`
/// impls. Doc comments placed above the invocation (e.g. a position table)
/// are forwarded onto the generated struct.
///
/// ```ignore
/// pos_event! {
///     /// Values for Event ID 1: committed.
///     struct CommittedValues uses committed_pos for Committed {
///         human_additions: u32 @ HUMAN_ADDITIONS,
///         tool_model_pairs: [String] @ TOOL_MODEL_PAIRS,
///     }
/// }
/// ```
macro_rules! pos_event {
    (
        $(#[$struct_meta:meta])*
        struct $name:ident uses $pos_mod:ident for $event_variant:ident {
            $( $field:ident : $ty:tt @ $pos_const:ident ),* $(,)?
        }
    ) => {
        $(#[$struct_meta])*
        #[derive(Debug, Clone, Default)]
        pub struct $name {
            $(
                pub $field: $crate::model::metrics::pos_encoded::PosField<
                    $crate::model::metrics::events::pos_event::pos_event_storage_ty!($ty),
                >,
            )*
        }

        impl $name {
            pub fn new() -> Self {
                Self::default()
            }

            $(
                pub fn $field(
                    mut self,
                    value: $crate::model::metrics::events::pos_event::pos_event_arg_ty!($ty),
                ) -> Self {
                    self.$field = Some(Some(value.into()));
                    self
                }

                paste::paste! {
                    #[allow(dead_code)]
                    pub fn [<$field _null>](mut self) -> Self {
                        self.$field = Some(None);
                        self
                    }
                }
            )*
        }

        impl $crate::model::metrics::pos_encoded::PosEncoded for $name {
            fn to_sparse(&self) -> $crate::model::metrics::types::SparseArray {
                let mut map = $crate::model::metrics::types::SparseArray::new();
                $(
                    $crate::model::metrics::pos_encoded::sparse_set(
                        &mut map,
                        $pos_mod::$pos_const,
                        $crate::model::metrics::events::pos_event::pos_event_to_json!(
                            $ty,
                            &self.$field
                        ),
                    );
                )*
                map
            }

            fn from_sparse(arr: &$crate::model::metrics::types::SparseArray) -> Self {
                Self {
                    $(
                        $field: $crate::model::metrics::events::pos_event::pos_event_from_sparse!(
                            $ty,
                            arr,
                            $pos_mod::$pos_const
                        ),
                    )*
                }
            }
        }

        impl $crate::model::metrics::types::EventValues for $name {
            fn event_id() -> $crate::model::metrics::types::MetricEventId {
                $crate::model::metrics::types::MetricEventId::$event_variant
            }

            fn to_sparse(&self) -> $crate::model::metrics::types::SparseArray {
                $crate::model::metrics::pos_encoded::PosEncoded::to_sparse(self)
            }

            fn from_sparse(arr: &$crate::model::metrics::types::SparseArray) -> Self {
                $crate::model::metrics::pos_encoded::PosEncoded::from_sparse(arr)
            }
        }
    };
}
pub(crate) use pos_event;
