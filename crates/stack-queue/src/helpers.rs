use crate::queue::INDEX_SHIFT;

pub(crate) const REGION_MASK: usize = 0b11111111;

#[cfg(target_pointer_width = "64")]
pub(crate) const REGION_COUNT: usize = 8;

#[cfg(target_pointer_width = "32")]
pub(crate) const REGION_COUNT: usize = 4;

/// 8 regions, 8 bits per region
#[inline(always)]
#[cfg(target_pointer_width = "64")]
pub(crate) const fn region_size<const N: usize>() -> usize {
  N << 3
}

/// 4 regions, 8 bits per region
#[inline(always)]
#[cfg(target_pointer_width = "32")]
pub(crate) const fn region_size<const N: usize>() -> usize {
  N << 2
}

/// Number of bits necessary to bitwise shift index into region
#[inline(always)]
#[cfg(target_pointer_width = "64")]
pub(crate) const fn region_shift<const N: usize>() -> usize {
  N.trailing_zeros() as usize - 3
}

/// Number of bits necessary to bitwise shift index into region
#[inline(always)]
#[cfg(target_pointer_width = "32")]
pub(crate) const fn region_shift<const N: usize>() -> usize {
  N.trailing_zeros() as usize - 2
}

/// Current occupancy region 0-7 or 0-3 depending on pointer width
#[inline(always)]
pub(crate) fn current_region<const N: usize>(index: usize) -> usize {
  index >> region_shift::<N>()
}

/// The 8-bit mask of the current region, used for extracting regional occupancy
#[inline(always)]
pub(crate) fn region_mask<const N: usize>(index: usize) -> usize {
  // Shift the region mask 8 bits per region
  REGION_MASK << (current_region::<N>(index) << REGION_COUNT.trailing_zeros())
}

/// Convert slot into an index by bitwise shifting away flag bits
#[inline(always)]
pub(crate) fn slot_index<const N: usize>(slot: usize) -> usize {
  // The bitwise right shift discards flags and gives us a counter that wraps around. The bitwise
  // AND gives us the modulus; this works because N will always be a power of 2, and so N - 1 will
  // always be a mask of N-bits set that can then extract N-bits, giving values 0..N in a loop
  // across all values in the counter and with usize::MAX being divisible by all values N
  slot >> INDEX_SHIFT & (N - 1)
}

/// One shifted relative to the current region
#[inline(always)]
pub(crate) fn one_shifted<const N: usize>(index: usize) -> usize {
  1 << ((index >> region_shift::<N>()) << REGION_COUNT.trailing_zeros())
}
