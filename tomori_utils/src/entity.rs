pub trait EntityRef: Copy + Eq {
    fn new(_: usize) -> Self;

    fn index(self) -> usize;
}

#[macro_export]
macro_rules! entity_impl {
    // Basic traits.
    ($entity:ident) => {
        impl $crate::EntityRef for $entity {
            #[inline]
            fn new(index: usize) -> Self {
                debug_assert!(index < (::core::u32::MAX as usize));
                $entity(index as u32)
            }

            #[inline]
            fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl $crate::packed_option::ReservedValue for $entity {
            #[inline]
            fn reserved_value() -> $entity {
                $entity(::core::u32::MAX)
            }

            #[inline]
            fn is_reserved_value(&self) -> bool {
                self.0 == ::core::u32::MAX
            }
        }

        impl $entity {
            /// Create a new instance from a `u32`.
            #[allow(dead_code, reason = "macro-generated code")]
            #[inline]
            pub fn from_u32(x: u32) -> Self {
                debug_assert!(x < ::core::u32::MAX);
                $entity(x)
            }

            /// Return the underlying index value as a `u32`.
            #[allow(dead_code, reason = "macro-generated code")]
            #[inline]
            pub fn as_u32(self) -> u32 {
                self.0
            }

            /// Return the raw bit encoding for this instance.
            #[allow(dead_code, reason = "macro-generated code")]
            #[inline]
            pub fn as_bits(self) -> u32 {
                self.0
            }

            /// Create a new instance from the raw bit encoding.
            #[allow(dead_code, reason = "macro-generated code")]
            #[inline]
            pub fn from_bits(x: u32) -> Self {
                $entity(x)
            }
        }
    };

    // Include basic `Display` impl using the given display prefix.
    ($entity:ident, $display_prefix:expr) => {
        $crate::entity_impl!($entity);

        impl ::core::fmt::Display for $entity {
            fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
                write!(f, concat!($display_prefix, "{}"), self.0)
            }
        }

        impl ::core::fmt::Debug for $entity {
            fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
                (self as &dyn ::core::fmt::Display).fmt(f)
            }
        }
    };

    // Alternate form for tuples we can't directly construct; providing "to" and "from" expressions
    // to turn an index *into* an entity, or get an index *from* an entity.
    ($entity:ident, $display_prefix:expr, $arg:ident, $to_expr:expr, $from_expr:expr) => {
        impl $crate::EntityRef for $entity {
            #[inline]
            fn new(index: usize) -> Self {
                debug_assert!(index < (::core::u32::MAX as usize));
                let $arg = index as u32;
                $to_expr
            }

            #[inline]
            fn index(self) -> usize {
                let $arg = self;
                $from_expr as usize
            }
        }

        impl $crate::packed_option::ReservedValue for $entity {
            #[inline]
            fn reserved_value() -> $entity {
                $entity::from_u32(::core::u32::MAX)
            }

            #[inline]
            fn is_reserved_value(&self) -> bool {
                self.as_u32() == ::core::u32::MAX
            }
        }

        impl $entity {
            /// Create a new instance from a `u32`.
            #[allow(dead_code, reason = "macro-generated code")]
            #[inline]
            pub fn from_u32(x: u32) -> Self {
                debug_assert!(x < ::core::u32::MAX);
                let $arg = x;
                $to_expr
            }

            /// Return the underlying index value as a `u32`.
            #[allow(dead_code, reason = "macro-generated code")]
            #[inline]
            pub fn as_u32(self) -> u32 {
                let $arg = self;
                $from_expr
            }
        }

        impl ::core::fmt::Display for $entity {
            fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
                write!(f, concat!($display_prefix, "{}"), self.as_u32())
            }
        }

        impl ::core::fmt::Debug for $entity {
            fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
                (self as &dyn ::core::fmt::Display).fmt(f)
            }
        }
    };
}
