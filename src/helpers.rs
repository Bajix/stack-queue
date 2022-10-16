use const_assert::{Assert, IsTrue};

use crate::queue::INDEX_SHIFT;

pub(crate) const REGION_MASK: usize = 0b11111111;

#[cfg(target_pointer_width = "64")]
pub(crate) const REGION_COUNT: usize = 8;

#[cfg(target_pointer_width = "32")]
pub(crate) const REGION_COUNT: usize = 4;

/// 8 regions, 8 bits per region
#[inline(always)]
#[cfg(target_pointer_width = "64")]
pub(crate) const fn region_size<const N: usize>() -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  N << 3
}

/// 4 regions, 8 bits per region
#[inline(always)]
#[cfg(target_pointer_width = "32")]
pub(crate) const fn region_size<const N: usize>() -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  N << 2
}

/// Number of bits necessary to bitwise shift index into region
#[inline(always)]
#[cfg(target_pointer_width = "64")]
pub(crate) const fn region_shift<const N: usize>() -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  N.trailing_zeros() as usize - 3
}

/// Number of bits necessary to bitwise shift index into region
#[inline(always)]
#[cfg(target_pointer_width = "32")]
pub(crate) const fn region_shift<const N: usize>() -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  N.trailing_zeros() as usize - 2
}

/// Current occupancy region 0-7 or 0-3 depending on pointer width
#[inline(always)]
pub(crate) fn current_region<const N: usize>(index: &usize) -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  index >> region_shift::<N>()
}

/// The 8-bit mask of the current region, used for extracting regional occupancy
#[inline(always)]
pub(crate) fn region_mask<const N: usize>(index: &usize) -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  // Shift the region mask 8 bits per region
  REGION_MASK << (current_region::<N>(index) << REGION_COUNT.trailing_zeros())
}

/// Convert slot into an index by bitwise shifting away flag bits
#[inline(always)]
pub(crate) fn slot_index<const N: usize>(slot: &usize) -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  // The bitwise right shift discards flags and gives us a counter that wraps around. The bitwise
  // AND gives us the modulus; this works because N will always be a power of 2, and so N - 1 will
  // always be a mask of N-bits set that can then extract N-bits, giving values 0..N in a loop
  // across all values in the counter and with usize::MAX being divisible by all values N
  slot >> INDEX_SHIFT & (N - 1)
}

// The phase bit of the current cycle, 0b1 or 0b10, as determined by the N-th bit shifted. By
// alternating the active phase bit when wrapping around this ensures that an out of phase batch
// pending assignment doesn't create side effects when converted into a bounded task assignment.
#[inline(always)]
pub(crate) fn active_phase_bit<const N: usize>(slot: &usize) -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  // (1 & (slot >> N.trailing_zeros() as usize + INDEX_SHIFT) extracts index & N as the 0 bit, which
  // then determines the current phase bit, 1 << 0 or 1 << 1, and alternates every N.
  1 << (1 & (slot >> (N.trailing_zeros() as usize + INDEX_SHIFT)))
}

/// One shifted relative to the current region
#[inline(always)]
pub(crate) fn one_shifted<const N: usize>(index: &usize) -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  1 << ((index >> region_shift::<N>()) << REGION_COUNT.trailing_zeros())
}

// A mask comprising of the active phase bit and the N-th bit shifted to align with the index
pub(crate) fn phase_mask<const N: usize>(slot: &usize) -> usize
where
  Assert<{ N == N.next_power_of_two() }>: IsTrue,
{
  active_phase_bit::<N>(slot) | (N << INDEX_SHIFT)
}
