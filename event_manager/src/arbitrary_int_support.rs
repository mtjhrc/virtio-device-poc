use crate::EventToken;
use arbitrary_int::{Number, UInt};

macro_rules! impl_for_ty {
    ($backing_type:ty) => {
        impl<const N: usize> EventToken for UInt<$backing_type, N> {
            // N < 64 is enforced by arbitrary_int crate
            const USED_BITS: u32 = N as u32;

            fn as_raw_token(&self) -> u64 {
                self.as_u64()
            }

            fn from_raw_token(data: u64) -> Self {
                Self::try_from(data).unwrap()
            }
        }
    };
}

impl_for_ty!(u8);
impl_for_ty!(u16);
impl_for_ty!(u32);
impl_for_ty!(u64);

#[cfg(test)]
mod tests {
    use super::EventToken;
    use arbitrary_int::{u33, u62, u63, Number};

    #[test]
    fn one_level() {
        #[derive(EventToken)]
        enum Tok {
            A(u33),
            B(u63),
        }
        assert_eq!(
            Tok::A(u33::MAX).as_raw_token(),
            1u64 << 33 | (u32::MAX as u64) << 1
        );
        assert_eq!(Tok::B(u63::MAX).as_raw_token(), u64::MAX);
    }

    #[test]
    fn two_levels() {
        #[derive(EventToken, Copy, Clone)]
        enum TokL1 {
            A(u33),
            B(u62),
        }

        #[derive(EventToken, Copy, Clone)]
        enum TokL2 {
            A(TokL1),
            B(TokL1),
        }

        assert_eq!(TokL2::A(TokL1::A(u33::new(0))).as_raw_token(), 0);
        assert_eq!(TokL2::B(TokL1::B(u62::MAX)).as_raw_token(), u64::MAX);
    }
}
