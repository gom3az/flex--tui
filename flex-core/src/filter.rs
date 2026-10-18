//! Hand-rolled scored fuzzy filter.
//!
//! The predicate is an ordered subsequence (case-insensitive): anything the
//! old subsequence matcher accepted stays accepted — tiers only reorder.
//! Tiers (frozen per `Docs/Implementation.md`):
//!
//! - [`TIER_PREFIX`] (`100`): the haystack starts with the needle. A
//!   string-prefix outranks a word-prefix: `"term"` must rank `"Terminal"`
//!   above `"Gnome Terminal"` (B-003 decision).
//! - [`TIER_PREFIX_WORD`] (`80`): the whole needle is a contiguous prefix
//!   of a non-first word (`"My Firm"` vs `"fir"`).
//! - [`TIER_RUN`] (`60`): a consecutive run of at least [`MIN_RUN`] needle
//!   chars matches contiguously elsewhere.
//! - [`TIER_WORD_BOUNDARY`] (`40`): greedy match whose first char sits on a
//!   word start (start of string or after a non-alphanumeric).
//! - [`TIER_SCATTERED`] (`10`): any other ordered-subsequence match.
//!
//! Penalties: `-2` per gap char (matched span minus needle length) and `-5`
//! per leading-offset char (char index of the first match), saturating at 0.
//!
//! NOTE (B-003, resolved): `Docs/Implementation.md` froze
//! `prefix 100 > prefix-word 80`; this module implements exactly that.
//!
//! The rank signature [`score`] is `nucleo`-swappable: [`Ranker`] is the
//! seam (swap [`FuzzyRanker`] for a `nucleo`-backed ranker without touching
//! [`rank_all`] or the key/render layers).

use std::collections::HashMap;

use crate::Row;

/// Haystack starts with the needle (`"fir"` in `"Firefox"`).
pub const TIER_PREFIX: u16 = 100;
/// Whole needle prefixes a non-first word (`"fir"` in `"My Firm"`).
pub const TIER_PREFIX_WORD: u16 = 80;
/// A consecutive run of [`MIN_RUN`]+ needle chars matches contiguously.
pub const TIER_RUN: u16 = 60;
/// First match sits on a word start, but no higher tier applies.
pub const TIER_WORD_BOUNDARY: u16 = 40;
/// Any other ordered-subsequence match.
pub const TIER_SCATTERED: u16 = 10;
/// Minimum consecutive-run length for [`TIER_RUN`].
pub const MIN_RUN: usize = 3;
/// Score penalty per gap char inside the matched span.
pub const GAP_PENALTY_PER_CHAR: u16 = 2;
/// Score penalty per leading-offset char (index of the first match).
pub const LEADING_PENALTY_PER_CHAR: u16 = 5;
/// Passthrough score used for empty-needle hits (order = provider order).
pub const EMPTY_NEEDLE_SCORE: u16 = u16::MAX;

/// Rank seam: implementors map `(needle, haystack)` to a score plus the
/// matched char positions (char offsets into `haystack`, ascending), or
/// `None` when the predicate rejects the pair.
///
/// `nucleo`-swap path: implement this trait for the nucleo matcher and pass
/// it to [`rank_all_with`]; [`score`] stays as the default engine.
pub trait Ranker {
    /// Rank one pair; see [`score`] for the default semantics.
    fn rank(&self, needle: &str, haystack: &str) -> Option<(u16, Vec<u32>)>;

    /// Rank one pair against a pre-folded needle.
    ///
    /// [`rank_all_with`] folds the needle once per query and calls this per
    /// row, so implementors must not re-fold `needle` here. The default
    /// unfolds back into a `String` and delegates to [`Ranker::rank`]
    /// (correct, but slow); the built-in rankers override this with
    /// streaming matchers that never materialize the haystack fold.
    fn rank_folded(&self, needle: &[char], haystack: &str) -> Option<(u16, Vec<u32>)> {
        let unfolded: String = needle.iter().collect();
        self.rank(&unfolded, haystack)
    }
}

/// Which ranking engine backs [`rank_all`] (see [`App::visible_rows`]).
///
/// `Spec` is the default tiered fuzzy engine ([`score`]); `Legacy` is the R1
/// escape hatch (`--filter-mode=legacy`): the ordered-subsequence predicate
/// with provider order preserved (no score reordering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    /// Tiered fuzzy: `prefix 100 > prefix-word 80 > run 60 > boundary 40 >
    /// scattered 10`, minus gap/offset penalties.
    #[default]
    Spec,
    /// Subsequence predicate only; every hit scores equally so the stable
    /// index tie-break keeps provider order.
    Legacy,
}

/// Default ordered-subsequence ranker behind [`score`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FuzzyRanker;

impl Ranker for FuzzyRanker {
    fn rank(&self, needle: &str, haystack: &str) -> Option<(u16, Vec<u32>)> {
        score(needle, haystack)
    }

    fn rank_folded(&self, needle: &[char], haystack: &str) -> Option<(u16, Vec<u32>)> {
        score_folded(needle, haystack)
    }
}

/// Legacy ordered-subsequence ranker (R1 escape hatch).
///
/// Acceptance matches [`score`] (case-insensitive ordered subsequence) but
/// every hit reports score `0` with no positions, so [`rank_all_with`]'s
/// index tie-break preserves provider order instead of reordering by tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegacyRanker;

impl Ranker for LegacyRanker {
    fn rank(&self, needle: &str, haystack: &str) -> Option<(u16, Vec<u32>)> {
        if needle.is_empty() {
            return Some((EMPTY_NEEDLE_SCORE, Vec::new()));
        }
        if is_subsequence(needle, haystack) {
            Some((0, Vec::new()))
        } else {
            None
        }
    }

    fn rank_folded(&self, needle: &[char], haystack: &str) -> Option<(u16, Vec<u32>)> {
        if needle.is_empty() {
            return Some((EMPTY_NEEDLE_SCORE, Vec::new()));
        }
        if is_subsequence_folded(needle, haystack) {
            Some((0, Vec::new()))
        } else {
            None
        }
    }
}

/// Score `(needle, haystack)`; `None` when `needle` is not an
/// ordered subsequence of `haystack` (case-insensitive).
///
/// Positions are char offsets into `haystack` in ascending order. An empty
/// needle trivially matches with [`EMPTY_NEEDLE_SCORE`] and no positions
/// (see [`rank_all`] for the order guarantee).
///
/// # Panics
///
/// Never panics on any input: internal indexing only touches the non-empty
/// greedy match vector built above it.
#[must_use]
pub fn score(needle: &str, haystack: &str) -> Option<(u16, Vec<u32>)> {
    if needle.is_empty() {
        return Some((EMPTY_NEEDLE_SCORE, Vec::new()));
    }
    let folded_needle: Vec<char> = lowercase_chars(needle);
    score_folded(&folded_needle, haystack)
}

/// [`score`] against a pre-folded needle (see [`Ranker::rank_folded`]).
///
/// The haystack fold streams (`chars().flat_map(to_lowercase)`) straight
/// into a buffer instead of the old `String` + `Vec<char>` double alloc;
/// tiers, penalties and positions match [`score`] exactly.
fn score_folded(needle: &[char], haystack: &str) -> Option<(u16, Vec<u32>)> {
    if needle.is_empty() {
        return Some((EMPTY_NEEDLE_SCORE, Vec::new()));
    }
    let folded_hay: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    if needle.len() > folded_hay.len() {
        return None;
    }
    let mut positions: Vec<u32> = Vec::with_capacity(needle.len());
    let tier_score = score_into(needle, &folded_hay, &mut positions)?;
    Some((tier_score, positions))
}

/// Tier core over pre-folded slices; fills `positions` on success.
///
/// `needle` is the once-per-query fold; `hay` is the streamed haystack fold.
/// Tier/penalty semantics are unchanged: prefix 100 > prefix-word 80 >
/// run>=3 60 > boundary 40 > scattered 10, subsequence required, gap/offset
/// penalties saturating at zero.
fn score_into(needle: &[char], hay: &[char], positions: &mut Vec<u32>) -> Option<u16> {
    debug_assert!(!needle.is_empty(), "empty needle is handled by the caller");
    positions.clear();
    // Fast path: single-char needles skip the word-prefix scan entirely.
    // The greedy first hit decides the tier (prefix / boundary / scattered).
    if needle.len() == 1 {
        let wanted = needle[0];
        // Tier 1: prefix (hay starts with needle).
        if !hay.is_empty() && hay[0] == wanted {
            positions.push(0);
            return Some(apply_penalties(TIER_PREFIX, 0, 0));
        }
        let mut first = None;
        for (index, &got) in hay.iter().enumerate() {
            if got == wanted {
                first = Some(index);
                break;
            }
        }
        let at = first?;
        positions.push(u32::try_from(at).unwrap_or(u32::MAX));
        let tier = if is_word_start(hay, at) {
            TIER_WORD_BOUNDARY
        } else {
            TIER_SCATTERED
        };
        return Some(apply_penalties(tier, at, 0));
    }
    // Tier 1: full prefix.
    if hay.starts_with(needle) {
        positions.extend((0..needle.len()).map(|i| u32::try_from(i).unwrap_or(u32::MAX)));
        return Some(apply_penalties(TIER_PREFIX, 0, 0));
    }
    // Tier 2: prefix of a non-first word (leftmost wins: lowest penalty).
    if let Some(word_start) = prefix_word_start(needle, hay) {
        positions.extend(
            (word_start..word_start + needle.len()).map(|i| u32::try_from(i).unwrap_or(u32::MAX)),
        );
        return Some(apply_penalties(TIER_PREFIX_WORD, word_start, 0));
    }
    // Greedy leftmost ordered-subsequence alignment. This is the acceptance
    // predicate: every pair matched here is `Some`; tiers only reorder.
    // (Known limit: greedy can miss a higher-tier alignment, e.g. needle
    // `"fir"` vs `"ffir"` scores scattered instead of prefix. Acceptance is
    // unaffected — only the tier is conservative.)
    positions.reserve(needle.len());
    let mut cursor = 0_usize;
    for &wanted in needle {
        let mut found = None;
        // Manual index loop: cheaper than `enumerate().skip(cursor)` and
        // lets the compiler bound-check once per row.
        let mut index = cursor;
        while index < hay.len() {
            if hay[index] == wanted {
                found = Some(index);
                cursor = index + 1;
                break;
            }
            index += 1;
        }
        positions.push(u32::try_from(found?).unwrap_or(u32::MAX));
    }
    let tier = if longest_run(positions) >= MIN_RUN {
        TIER_RUN
    } else if is_word_start(hay, positions[0] as usize) {
        TIER_WORD_BOUNDARY
    } else {
        TIER_SCATTERED
    };
    let first = positions[0] as usize;
    let last = positions[positions.len() - 1] as usize;
    let gaps = last - first + 1 - needle.len();
    Some(apply_penalties(tier, first, gaps))
}

/// One ranked row: provider index, score, and match positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedHit {
    /// Index into the provider's row slice.
    pub index: usize,
    /// Tier minus penalties (or [`EMPTY_NEEDLE_SCORE`] on empty needle).
    pub score: u16,
    /// Ascending char-offset match positions (empty on empty needle).
    pub positions: Vec<u32>,
}

/// Re-rank `rows` against `needle` with the default [`FuzzyRanker`].
///
/// - Empty needle: every row in provider order (no scoring).
/// - Otherwise: matches ordered by score descending, ties broken by the
///   original provider index ascending (stable, deterministic).
///
/// Fast path (OPT-1): the needle folds once per query and the haystack fold
/// plus the `positions` scratch are reused across rows, so per-row work is
/// matching only — no `String`/`Vec` allocs except the per-hit positions.
#[must_use]
pub fn rank_all(needle: &str, rows: &[Row]) -> Vec<RankedHit> {
    rank_all_inner(needle, rows, None)
}

/// [`rank_all`] with a zoxide-style frecency overlay.
///
/// Acceptance, tiers and penalties are identical; the sort gains a frecency
/// tie-break between equal tiers (see [`usage_score`]). Rows never recorded
/// score `0.0` and keep provider order among themselves, so an empty table
/// ranks exactly like [`rank_all`]. On an empty needle every row matches and
/// frecency alone decides the order (most-used on top).
#[must_use]
pub fn rank_all_scored(needle: &str, rows: &[Row], usage: &UsageTable, now: u64) -> Vec<RankedHit> {
    rank_all_inner(needle, rows, Some((usage, now)))
}

/// Shared matching loop behind [`rank_all`] and [`rank_all_scored`].
fn rank_all_inner(needle: &str, rows: &[Row], usage: Option<(&UsageTable, u64)>) -> Vec<RankedHit> {
    if needle.is_empty() {
        let mut hits = empty_hits(rows);
        sort_hits(&mut hits, rows, usage);
        return hits;
    }
    let folded_needle: Vec<char> = lowercase_chars(needle);
    // Upper-bound reserve: at most one hit per row; avoids regrow on
    // broad queries like "a" over the 3200-row corpus.
    let mut hits: Vec<RankedHit> = Vec::with_capacity(rows.len());
    let mut folded_hay: Vec<char> = Vec::new();
    let mut positions: Vec<u32> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        folded_hay.clear();
        folded_hay.extend(row.label.chars().flat_map(char::to_lowercase));
        if folded_needle.len() > folded_hay.len() {
            continue;
        }
        if let Some(tier_score) = score_into(&folded_needle, &folded_hay, &mut positions) {
            hits.push(RankedHit {
                index,
                score: tier_score,
                positions: positions.clone(),
            });
        }
    }
    sort_hits(&mut hits, rows, usage);
    hits
}

/// [`rank_all`] over an explicit [`Ranker`] (the `nucleo` seam).
///
/// The needle folds once per query and goes into the ranker as `&[char]`
/// (see [`Ranker::rank_folded`]); per-row work never re-folds it.
#[must_use]
pub fn rank_all_with<R: Ranker>(ranker: &R, needle: &str, rows: &[Row]) -> Vec<RankedHit> {
    if needle.is_empty() {
        return empty_hits(rows);
    }
    let folded_needle: Vec<char> = lowercase_chars(needle);
    let mut hits: Vec<RankedHit> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            ranker
                .rank_folded(&folded_needle, &row.label)
                .map(|(score, positions)| RankedHit {
                    index,
                    score,
                    positions,
                })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index)));
    hits
}

/// [`rank_all`] over the [`LegacyRanker`] (R1 escape hatch).
///
/// Same subsequence predicate as [`score`], but every hit scores `0`, so
/// the index tie-break preserves provider order (no score reordering).
#[must_use]
pub fn rank_all_legacy(needle: &str, rows: &[Row]) -> Vec<RankedHit> {
    rank_all_with(&LegacyRanker, needle, rows)
}

/// Empty-needle hits: every row in provider order (no scoring).
fn empty_hits(rows: &[Row]) -> Vec<RankedHit> {
    let mut hits = Vec::with_capacity(rows.len());
    hits.extend(rows.iter().enumerate().map(|(index, _)| RankedHit {
        index,
        score: EMPTY_NEEDLE_SCORE,
        positions: Vec::new(),
    }));
    hits
}

fn lowercase_chars(text: &str) -> Vec<char> {
    // Single pass: stream `to_lowercase` straight into the Vec instead of
    // `to_lowercase() -> String -> chars().collect()` (two allocs).
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Whether `needle` is a case-insensitive ordered subsequence of `haystack`.
///
/// This is the shared acceptance predicate: [`score`] and [`LegacyRanker`]
/// agree on *whether* a pair matches and differ only in scoring.
#[must_use]
pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    is_subsequence_folded(&lowercase_chars(needle), haystack)
}

/// [`is_subsequence`] against a pre-folded needle.
///
/// Streams the haystack fold (`chars().flat_map(to_lowercase)`) with no
/// allocation; the predicate matches [`score`] exactly.
fn is_subsequence_folded(needle: &[char], haystack: &str) -> bool {
    let mut wanted = needle.iter();
    let Some(first) = wanted.next() else {
        return true;
    };
    let mut current = *first;
    for got in haystack.chars().flat_map(char::to_lowercase) {
        if got == current {
            if let Some(next) = wanted.next() {
                current = *next;
            } else {
                return true;
            }
        }
    }
    false
}

/// Leftmost non-first word start where `needle` matches contiguously.
fn prefix_word_start(needle: &[char], haystack: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    for start in 1..=haystack.len() - needle.len() {
        if is_word_start(haystack, start) && haystack[start..start + needle.len()] == *needle {
            return Some(start);
        }
    }
    None
}

/// Whether `index` starts a word (string start or non-alphanumeric before).
fn is_word_start(haystack: &[char], index: usize) -> bool {
    index == 0 || !haystack[index - 1].is_alphanumeric()
}

/// Length of the longest run of consecutive positions.
fn longest_run(positions: &[u32]) -> usize {
    let mut best = 1_usize;
    let mut run = 1_usize;
    for pair in positions.windows(2) {
        if pair[1] == pair[0] + 1 {
            run += 1;
            best = best.max(run);
        } else {
            run = 1;
        }
    }
    best
}

/// Subtract gap/leading penalties, saturating at zero.
fn apply_penalties(tier: u16, leading: usize, gaps: usize) -> u16 {
    let gap_hit = u16::try_from(gaps).unwrap_or(u16::MAX);
    let lead_hit = u16::try_from(leading).unwrap_or(u16::MAX);
    tier.saturating_sub(GAP_PENALTY_PER_CHAR.saturating_mul(gap_hit))
        .saturating_sub(LEADING_PENALTY_PER_CHAR.saturating_mul(lead_hit))
}

/// Zoxide-style frecency overlay (usage ranking).
///
/// The fuzzy predicate and tiers above gate *whether* a row matches; this
/// section only reorders matches by learned use, mirroring
/// `zoxide/src/db/dir.rs`:
/// - [`usage_score`] = `rank × boost`, boost `4.0`/`2.0`/`0.5`/`0.25` for
///   entries used within the hour/day/week or earlier.
/// - [`usage_record`] = `rank += 1.0`, `last_accessed = now` (new ids start
///   at `1.0`), the `add_update` port.
/// - [`usage_age`] trims the table when it exceeds [`MAX_USAGE_ENTRIES`]
///   (scale `×0.9`, drop `rank < 1.0`), the `age` port.
/// - [`usage_remove`] drops one id, the `remove` port.
///
/// Keyed by row-id string (not [`crate::RowId`]: it has no `Borrow<str>`,
/// so `get(id_str)` would not compile). Scores are `f64` compared with
/// `total_cmp`; ranks stay non-negative so `NaN` cannot arise.
///
/// Nowhere here touches I/O or the clock: the caller supplies `now`
/// (epoch seconds) and owns persistence (see `flex-rice/src/usage.rs`).
/// Seconds in an hour (frecency recency boundary, cf. zoxide `HOUR`).
pub const FRECENCY_HOUR_SECS: u64 = 3_600;
/// Seconds in a day (frecency recency boundary, cf. zoxide `DAY`).
pub const FRECENCY_DAY_SECS: u64 = 86_400;
/// Seconds in a week (frecency recency boundary, cf. zoxide `WEEK`).
pub const FRECENCY_WEEK_SECS: u64 = 604_800;
/// Score boost for entries used within the hour (cf. zoxide `4.0`).
pub const FRECENCY_BOOST_HOUR: f64 = 4.0;
/// Score boost for entries used within the day (cf. zoxide `2.0`).
pub const FRECENCY_BOOST_DAY: f64 = 2.0;
/// Score boost for entries used within the week (cf. zoxide `0.5`).
pub const FRECENCY_BOOST_WEEK: f64 = 0.5;
/// Score boost for older entries (cf. zoxide `0.25`).
pub const FRECENCY_BOOST_OLDER: f64 = 0.25;
/// Rank added per recorded choice (cf. zoxide `add_update(path, 1.0, now)`).
pub const USAGE_RANK_PER_USE: f64 = 1.0;
/// Minimum surviving rank after aging (cf. zoxide's `rank < 1.0` drop).
pub const USAGE_MIN_RANK: f64 = 1.0;
/// Aging scale applied when the table overflows (cf. zoxide `0.9`).
pub const USAGE_AGE_FACTOR: f64 = 0.9;
/// Maximum usage entries kept per provider (bounds the store file).
pub const MAX_USAGE_ENTRIES: usize = 512;

/// One learned entry: zoxide `Dir` minus the path (the row id is the map key).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageEntry {
    /// Accumulated uses (`+1.0` per recorded choice, aged on overflow).
    pub rank: f64,
    /// Epoch seconds of the last recorded choice.
    pub last_accessed: u64,
}

/// Usage table keyed by row-id string (see module docs for why not `RowId`).
pub type UsageTable = HashMap<String, UsageEntry>;

/// Zoxide `Dir::score` port: `rank × recency boost` at `now` (epoch seconds).
///
/// Negative ranks (only possible from hand-built tables) clamp to zero.
#[must_use]
pub fn usage_score(entry: &UsageEntry, now: u64) -> f64 {
    let duration = now.saturating_sub(entry.last_accessed);
    let boost = if duration < FRECENCY_HOUR_SECS {
        FRECENCY_BOOST_HOUR
    } else if duration < FRECENCY_DAY_SECS {
        FRECENCY_BOOST_DAY
    } else if duration < FRECENCY_WEEK_SECS {
        FRECENCY_BOOST_WEEK
    } else {
        FRECENCY_BOOST_OLDER
    };
    entry.rank.max(0.0) * boost
}

/// Zoxide `add_update` port: bump `id` by one use at `now`, inserting at
/// rank `1.0` when unseen.
pub fn usage_record(table: &mut UsageTable, id: &str, now: u64) {
    match table.get_mut(id) {
        Some(entry) => {
            entry.rank = (entry.rank + USAGE_RANK_PER_USE).max(0.0);
            entry.last_accessed = now;
        }
        None => {
            table.insert(
                id.to_owned(),
                UsageEntry {
                    rank: USAGE_RANK_PER_USE.max(0.0),
                    last_accessed: now,
                },
            );
        }
    }
}

/// Zoxide `remove` port: drop `id` from the table; returns whether one existed.
pub fn usage_remove(table: &mut UsageTable, id: &str) -> bool {
    table.remove(id).is_some()
}

/// Zoxide `age` port (entry-count budget): when the table exceeds
/// [`MAX_USAGE_ENTRIES`], scale every rank by [`USAGE_AGE_FACTOR`] and drop
/// entries below [`USAGE_MIN_RANK`]. Under budget this is a no-op.
pub fn usage_age(table: &mut UsageTable) {
    if table.len() <= MAX_USAGE_ENTRIES {
        return;
    }
    table.retain(|_, entry| {
        entry.rank *= USAGE_AGE_FACTOR;
        entry.rank >= USAGE_MIN_RANK
    });
}

/// Frecency of one ranked hit (`0.0` when the row was never recorded).
fn hit_frecency(hit: &RankedHit, rows: &[Row], usage: &UsageTable, now: u64) -> f64 {
    rows.get(hit.index)
        .and_then(|row| usage.get(row.id.as_str()))
        .map_or(0.0, |entry| usage_score(entry, now))
}

/// Sort hits by tier descending, then frecency descending, then provider
/// index ascending (stable, deterministic).
fn sort_hits(hits: &mut [RankedHit], rows: &[Row], usage: Option<(&UsageTable, u64)>) {
    match usage {
        Some((table, now)) => {
            // Pre-resolve frecency once per hit: the old comparator did two
            // `HashMap<String,_>` lookups per comparison (O(log n) lookups
            // per hit). Cache scores in a side table indexed by position.
            let mut frecency: Vec<f64> = Vec::with_capacity(hits.len());
            frecency.extend(hits.iter().map(|hit| hit_frecency(hit, rows, table, now)));
            // Sort indices to keep `hits` move-cheap, then reorder in place.
            let mut order: Vec<usize> = (0..hits.len()).collect();
            order.sort_by(|&a, &b| {
                hits[b]
                    .score
                    .cmp(&hits[a].score)
                    .then_with(|| frecency[b].total_cmp(&frecency[a]))
                    .then_with(|| hits[a].index.cmp(&hits[b].index))
            });
            // Apply permutation via a single clone of the slice (one alloc,
            // no per-compare HashMap traffic).
            let sorted: Vec<RankedHit> = order.into_iter().map(|i| hits[i].clone()).collect();
            hits.clone_from_slice(&sorted);
        }
        None => hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RowId;

    fn row(label: &str) -> Row {
        Row::new(RowId::new(label), label)
    }

    #[test]
    fn prefix_beats_scattered_for_fir() {
        let firefox = score("fir", "Firefox").expect("Firefox matches fir");
        assert_eq!(firefox.0, TIER_PREFIX);
        assert_eq!(firefox.1, vec![0, 1, 2]);
    }

    #[test]
    fn legacy_accepts_exactly_the_spec_predicate() {
        let ranker = LegacyRanker;
        for (needle, haystack) in [
            ("fir", "Firefox"),
            ("FIR", "Firefox"),
            ("fx", "Firefox"),
            ("fir", "Confirm"),
            ("fir", "My Firm"),
            ("zzz", "Firefox"),
            ("firefoxes", "Firefox"),
        ] {
            assert_eq!(
                ranker.rank(needle, haystack).is_some(),
                score(needle, haystack).is_some(),
                "predicate must agree for {needle:?} vs {haystack:?}"
            );
        }
    }

    #[test]
    fn legacy_preserves_provider_order_while_spec_reorders() {
        // Provider order is deliberately anti-score: spec must rank
        // "Firefox" (prefix 100) > "My Firm" (prefix-word 80-15=65) >
        // "Confirm" (run 60-15=45), while legacy keeps insertion order.
        let rows = vec![row("Confirm"), row("Firefox"), row("My Firm")];
        let legacy: Vec<usize> = rank_all_legacy("fir", &rows)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(legacy, vec![0, 1, 2], "legacy keeps provider order");
        let spec: Vec<usize> = rank_all("fir", &rows)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(spec, vec![1, 2, 0], "spec reorders by tier");
    }

    #[test]
    fn legacy_rejects_non_subsequences() {
        let rows = vec![row("Firefox"), row("Htop")];
        let hits = rank_all_legacy("fir", &rows);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].index, 0);
    }

    fn usage_table(entries: &[(&str, f64, u64)]) -> UsageTable {
        entries
            .iter()
            .map(|(id, rank, last)| {
                (
                    (*id).to_owned(),
                    UsageEntry {
                        rank: *rank,
                        last_accessed: *last,
                    },
                )
            })
            .collect()
    }

    // Exact `==` below: ranks are small integers and boosts are powers of
    // two, so every compared product is exactly representable.
    #[allow(clippy::float_cmp)]
    #[test]
    fn usage_score_boost_boundaries_mirror_zoxide() {
        let now = 1_000_000_u64;
        let at = |ago: u64| UsageEntry {
            rank: 2.0,
            last_accessed: now - ago,
        };
        let cases = [
            (0, 8.0, "within the hour: x4"),
            (FRECENCY_HOUR_SECS - 1, 8.0, "hour edge inclusive"),
            (FRECENCY_HOUR_SECS, 4.0, "within the day: x2"),
            (FRECENCY_DAY_SECS - 1, 4.0, "day edge inclusive"),
            (FRECENCY_DAY_SECS, 1.0, "within the week: x0.5"),
            (FRECENCY_WEEK_SECS - 1, 1.0, "week edge inclusive"),
            (FRECENCY_WEEK_SECS, 0.5, "older: x0.25"),
        ];
        for (ago, expected, what) in cases {
            assert!(
                usage_score(&at(ago), now) == expected,
                "{what}: got {}, want {expected}",
                usage_score(&at(ago), now)
            );
        }
        // Future timestamps saturate instead of underflowing.
        let future = UsageEntry {
            rank: 2.0,
            last_accessed: now + 60,
        };
        assert_eq!(usage_score(&future, now), 8.0);
    }

    #[test]
    fn usage_record_accumulates_and_stamps() {
        let mut table = UsageTable::new();
        usage_record(&mut table, "a", 100);
        assert_eq!(
            table["a"],
            UsageEntry {
                rank: 1.0,
                last_accessed: 100
            },
            "new ids start at rank 1.0"
        );
        usage_record(&mut table, "a", 200);
        assert_eq!(
            table["a"],
            UsageEntry {
                rank: 2.0,
                last_accessed: 200
            },
            "+1.0 per use, timestamp moves"
        );
    }

    #[test]
    fn usage_remove_reports_presence() {
        let mut table = usage_table(&[("a", 3.0, 100)]);
        assert!(usage_remove(&mut table, "a"));
        assert!(!usage_remove(&mut table, "a"));
        assert!(table.is_empty());
    }

    // Aged ranks compared here are exact (10.0*0.9=9.0, 100.0*0.9=90.0
    // are exactly representable), so `assert_eq!` is sound.
    #[allow(clippy::float_cmp)]
    #[test]
    fn usage_age_only_fires_over_budget() {
        let mut small = usage_table(&[("a", 50.0, 1)]);
        usage_age(&mut small);
        assert_eq!(small["a"].rank, 50.0, "under budget is a no-op");
        let mut big: UsageTable = (0..=MAX_USAGE_ENTRIES)
            .map(|i| {
                (
                    format!("id{i:04}"),
                    UsageEntry {
                        rank: 10.0,
                        last_accessed: 1,
                    },
                )
            })
            .collect();
        usage_age(&mut big);
        assert!(
            big.values().all(|entry| entry.rank == 9.0),
            "over budget scales every rank by 0.9"
        );
    }

    // Aged ranks compared here are exact (10.0*0.9=9.0, 100.0*0.9=90.0
    // are exactly representable), so `assert_eq!` is sound.
    #[allow(clippy::float_cmp)]
    #[test]
    fn usage_age_drops_below_minimum_rank() {
        let mut table: UsageTable = (0..MAX_USAGE_ENTRIES)
            .map(|i| {
                (
                    format!("id{i:04}"),
                    UsageEntry {
                        rank: 1.0,
                        last_accessed: 1,
                    },
                )
            })
            .collect();
        table.insert(
            "fresh".to_owned(),
            UsageEntry {
                rank: 100.0,
                last_accessed: 1,
            },
        );
        usage_age(&mut table);
        assert!(!table.contains_key("id0000"), "1.0 * 0.9 < 1.0 drops");
        assert_eq!(table["fresh"].rank, 90.0);
    }

    #[test]
    fn scored_empty_table_matches_plain_order() {
        let rows = vec![row("Confirm"), row("Firefox"), row("My Firm")];
        let plain: Vec<usize> = rank_all("fir", &rows)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        let scored: Vec<usize> = rank_all_scored("fir", &rows, &UsageTable::new(), 1_000_000)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(scored, plain, "no learned entries, no reorder");
        let empty_plain: Vec<usize> = rank_all("", &rows)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        let empty_scored: Vec<usize> = rank_all_scored("", &rows, &UsageTable::new(), 1_000_000)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(empty_scored, empty_plain);
        assert_eq!(
            empty_scored,
            vec![0, 1, 2],
            "empty needle keeps provider order"
        );
    }

    #[test]
    fn scored_prefers_used_row_within_tier() {
        // Both prefix a non-first word at index 3: tier 80, leading 15 → 65
        // each. The recorded one must lead on frecency alone.
        let rows = vec![row("My Firm"), row("My First")];
        let plain: Vec<usize> = rank_all("fir", &rows)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(plain, vec![0, 1], "true tier tie keeps provider order");
        let now = 1_000_000_u64;
        let table = usage_table(&[("My First", 5.0, now)]);
        let order: Vec<usize> = rank_all_scored("fir", &rows, &table, now)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(order, vec![1, 0], "frecency breaks the tier tie");
    }

    #[test]
    fn scored_never_outranks_a_higher_tier() {
        // B-003 holds under frecency: string-prefix still beats a heavily
        // used scattered/word match.
        let rows = vec![row("Gnome Terminal"), row("Terminal")];
        let now = 1_000_000_u64;
        let table = usage_table(&[("Gnome Terminal", 100.0, now)]);
        let order: Vec<usize> = rank_all_scored("term", &rows, &table, now)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(order, vec![1, 0], "prefix 100 beats used prefix-word");
    }

    #[test]
    fn scored_empty_needle_orders_by_frecency() {
        let rows = vec![row("aa"), row("bb"), row("cc")];
        let now = 1_000_000_u64;
        let table = usage_table(&[("cc", 1.0, now - FRECENCY_WEEK_SECS), ("bb", 3.0, now)]);
        let order: Vec<usize> = rank_all_scored("", &rows, &table, now)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        // bb: 3*4=12, cc: 1*0.25=0.25, aa: unrecorded 0.0.
        assert_eq!(order, vec![1, 2, 0], "most-used on top at open");
    }

    #[test]
    fn scored_balances_count_against_recency() {
        let rows = vec![row("aa"), row("bb")];
        let now = 10_000_000_u64;
        // aa: 50*0.25=12.5, bb: 2*4=8 — heavy stale use still leads.
        let table = usage_table(&[("aa", 50.0, now - FRECENCY_WEEK_SECS), ("bb", 2.0, now)]);
        let order: Vec<usize> = rank_all_scored("", &rows, &table, now)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(order, vec![0, 1]);
        // aa: 12.5, bb: 4*4=16 — a fresh run of uses overtakes it.
        let table = usage_table(&[("aa", 50.0, now - 2 * FRECENCY_WEEK_SECS), ("bb", 4.0, now)]);
        let order: Vec<usize> = rank_all_scored("", &rows, &table, now)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        assert_eq!(order, vec![1, 0], "recent use beats stale count");
    }
}
