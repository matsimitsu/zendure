//! Price tiers and 3-hour windows for one displayed day.
//!
//! Pure functions over plain inputs: a day is a slice of local-hour slots, each
//! `Some(price)` or `None` for an hour the feed has no price for. The slice
//! length is the day's real slot count (23, 24 or 25), so DST days need no
//! special case, and a block can never cross midnight because the slice ends.

use crate::units::CentsPerKwh;

/// Hours in a cheapest/priciest block.
const BLOCK_SLOTS: usize = 3;

/// Index of a local-hour slot within the displayed day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot(pub usize);

/// The prices a day's tiers split at: cheap below `lo`, expensive from `hi`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub lo: CentsPerKwh,
    pub hi: CentsPerKwh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Cheap,
    Normal,
    Expensive,
}

impl Tier {
    /// Suffix of the `price-tier--*` CSS class and the `data-tier` attribute.
    pub fn class_suffix(self) -> &'static str {
        match self {
            Tier::Cheap => "cheap",
            Tier::Normal => "normal",
            Tier::Expensive => "expensive",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tier::Cheap => "Cheap",
            Tier::Normal => "Normal",
            Tier::Expensive => "Expensive",
        }
    }

    /// Cheap is strictly below `lo`, expensive is `hi` and up. Being paid to
    /// import is cheap however the rest of the day ranks, and a day with no
    /// spread (`lo == hi`) has nothing to flag, so it reads as normal rather
    /// than wholly expensive.
    pub fn of(price: CentsPerKwh, Thresholds { lo, hi }: Thresholds) -> Tier {
        if price < lo || price < CentsPerKwh(0.0) {
            Tier::Cheap
        } else if price >= hi && lo < hi {
            Tier::Expensive
        } else {
            Tier::Normal
        }
    }
}

/// The thresholds at the one-third and two-thirds ranks of the priced hours;
/// `None` when there are none.
pub fn tiers(prices: &[CentsPerKwh]) -> Option<Thresholds> {
    let mut sorted = prices.to_vec();
    // total_cmp: a NaN must not panic the dashboard render.
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let n = sorted.len();
    Some(Thresholds {
        lo: *sorted.get(n / 3)?,
        hi: *sorted.get(2 * n / 3)?,
    })
}

/// A 3-hour block and its mean price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Block {
    pub start: Slot,
    pub mean: CentsPerKwh,
}

impl Block {
    /// One past the last slot of the block.
    pub fn end(self) -> Slot {
        Slot(self.start.0 + BLOCK_SLOTS)
    }
}

/// The block with the lowest mean starting at or after `from`. Ties go to the
/// earliest block.
pub fn cheapest_block(day: &[Option<CentsPerKwh>], from: Slot) -> Option<Block> {
    best_block(day, from, |candidate, best| candidate < best)
}

/// The block with the highest mean starting at or after `from`. Ties go to the
/// earliest block.
pub fn priciest_block(day: &[Option<CentsPerKwh>], from: Slot) -> Option<Block> {
    best_block(day, from, |candidate, best| candidate > best)
}

fn best_block(
    day: &[Option<CentsPerKwh>],
    from: Slot,
    better: impl Fn(CentsPerKwh, CentsPerKwh) -> bool,
) -> Option<Block> {
    let mut best: Option<Block> = None;
    for (start, window) in day.windows(BLOCK_SLOTS).enumerate().skip(from.0) {
        // A block with an unpriced hour has no honest mean.
        let Some(prices) = window.iter().copied().collect::<Option<Vec<_>>>() else {
            continue;
        };
        let mean = CentsPerKwh::mean(prices)?;
        if best.is_none_or(|b| better(mean, b.mean)) {
            best = Some(Block {
                start: Slot(start),
                mean,
            });
        }
    }
    best
}

#[cfg(test)]
#[path = "tiers_tests.rs"]
mod tests;
