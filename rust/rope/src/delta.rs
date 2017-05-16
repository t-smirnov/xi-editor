// Copyright 2016 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A data structure for representing editing operations on ropes.
//! It's useful to explicitly represent these operations so they can be
//! shared across multiple subsystems.

use interval::Interval;
use tree::{Node, NodeInfo, TreeBuilder};
use subset::{Subset, SubsetBuilder};
use std::cmp::min;
use std::ops::Deref;
use std::fmt;

#[derive(Clone)]
enum DeltaElement<N: NodeInfo> {
    /// Represents a range of text in the base document. Includes beginning, excludes end.
    Copy(usize, usize),  // note: for now, we lose open/closed info at interval endpoints
    Insert(Node<N>),
}

/// Represents changes to a document by describing the new document as a
/// sequence of sections copied from the old document and of new inserted
/// text. Deletions are represented by gaps in the ranges copied from the old
/// document.
///
/// For example, Editing "abcd" into "acde" could be represented as:
/// `[Copy(0,1),Copy(2,4),Insert("e")]`
#[derive(Clone)]
pub struct Delta<N: NodeInfo> {
    els: Vec<DeltaElement<N>>,
    base_len: usize,
}

/// A struct marking that a Delta contains only insertions. That is, it copies
/// all of the old document in the same order. It has a `Deref` impl so all
/// normal `Delta` methods can also be used on it.
pub struct InsertDelta<N: NodeInfo>(Delta<N>);

impl<N: NodeInfo> Delta<N> {
    pub fn simple_edit(interval: Interval, rope: Node<N>, base_len: usize) -> Delta<N> {
        let mut builder = Builder::new(base_len);
        if rope.len() > 0 {
            builder.replace(interval, rope);
        } else {
            builder.delete(interval);
        }
        builder.build()
    }

    /// Apply the delta to the given rope. May not work well if the length of the rope
    /// is not compatible with the construction of the delta.
    pub fn apply(&self, base: &Node<N>) -> Node<N> {
        debug_assert_eq!(base.len(), self.base_len, "must apply Delta to Node of correct length");
        let mut b = TreeBuilder::new();
        for elem in &self.els {
            match *elem {
                DeltaElement::Copy(beg, end) => {
                    base.push_subseq(&mut b, Interval::new_closed_open(beg, end))
                }
                DeltaElement::Insert(ref n) => b.push(n.clone())
            }
        }
        b.build()
    }

    /// Factor the delta into an insert-only delta and a subset representing deletions.
    /// Applying the insert then the delete yields the same result as the original delta:
    ///
    /// ```no_run
    /// # use xi_rope::rope::{Rope, RopeInfo};
    /// # use xi_rope::delta::Delta;
    /// # use std::str::FromStr;
    /// fn test_factor(d : &Delta<RopeInfo>, r : &Rope) {
    ///     let (ins, del) = d.clone().factor();
    ///     let del2 = del.transform_expand(&ins.inserted_subset());
    ///     assert_eq!(String::from(del2.delete_from(&ins.apply(r))), String::from(d.apply(r)));
    /// }
    /// ```
    pub fn factor(self) -> (InsertDelta<N>, Subset) {
        let mut ins = Vec::new();
        let mut sb = SubsetBuilder::new();
        let mut b1 = 0;
        let mut e1 = 0;
        for elem in self.els {
            match elem {
                DeltaElement::Copy(b, e) => {
                    sb.add_range(e1, b);
                    e1 = e;
                }
                DeltaElement::Insert(n) => {
                    if e1 > b1 {
                        ins.push(DeltaElement::Copy(b1, e1));
                    }
                    b1 = e1;
                    ins.push(DeltaElement::Insert(n));
                }
            }
        }
        if b1 < self.base_len {
            ins.push(DeltaElement::Copy(b1, self.base_len));
        }
        sb.add_range(e1, self.base_len);
        (InsertDelta(Delta { els: ins, base_len: self.base_len }), sb.build())
    }

    /// Synthesize a delta from a "union string" and two subsets, an old set
    /// of deletions and a new set of deletions from the union. The Delta is
    /// from text to text, not union to union; anything in both subsets will
    /// be assumed to be missing from the Delta base and the new text. You can
    /// also think of these as a set of insertions and one of deletions, with
    /// overlap doing nothing. This is basically the inverse of `factor`.
    ///
    /// Since only the deleted portions of the union string are necessary,
    /// instead of requiring a union string the function takes a `tombstones`
    /// rope which contains the deleted portions of the union string, and a
    /// `tombstone_dels` subset which identifies the segments of the union
    /// string which are deleted and thus correspond to the tombstones.
    ///
    /// ```no_run
    /// # use xi_rope::rope::{Rope, RopeInfo};
    /// # use xi_rope::delta::Delta;
    /// # use std::str::FromStr;
    /// fn test_synthesize(d : &Delta<RopeInfo>, r : &Rope) {
    ///     let (ins_d, del) = d.clone().factor();
    ///     let ins = ins_d.inserted_subset();
    ///     let del2 = del.transform_expand(&ins);
    ///     let r2 = ins_d.apply(&r);
    ///     let tombstones = ins.complement(r2.len()).delete_from(&r2);
    ///     let d2 = Delta::synthesize(&tombstones, &ins, r2.len(), &ins, &del);
    ///     assert_eq!(String::from(d2.apply(r)), String::from(d.apply(r)));
    /// }
    /// ```
    pub fn synthesize(tombstones: &Node<N>, tombstone_dels: &Subset, union_len: usize, from_dels: &Subset, to_dels: &Subset) -> Delta<N> {
        let base_len = from_dels.len_after_delete(union_len);
        let mut els = Vec::new();
        let mut x = 0;
        let mut old_ranges = from_dels.complement_iter(union_len);
        let mut last_old = old_ranges.next();
        let mut m = tombstone_dels.mapper();
        // For each segment of the new text
        for (b, e) in to_dels.complement_iter(union_len) {
            // Fill the whole segment
            let mut beg = b;
            while beg < e {
                // Skip over ranges in old text until one overlaps where we want to fill
                while let Some((ib, ie)) = last_old {
                    if ie > beg {
                        break;
                    }
                    x += ie - ib;
                    last_old = old_ranges.next();
                }
                // If we have a range in the old text with the character at beg, then we Copy
                if last_old.is_some() && last_old.unwrap().0 <= beg {
                    let (ib, ie) = last_old.unwrap();
                    let end = min(e, ie);
                    // Try to merge contigous Copys in the output
                    let xbeg = beg + x - ib;  // "beg - ib + x" better for overflow?
                    let xend = end + x - ib;  // ditto
                    let merged = if let Some(&mut DeltaElement::Copy(_, ref mut le)) = els.last_mut() {
                        if *le == xbeg {
                            *le = xend;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !merged {
                        els.push(DeltaElement::Copy(xbeg, xend));
                    }
                    beg = end;
                } else { // if the character at beg isn't in the old text, then we Insert
                    // Insert up until the next old range we could Copy from, or the end of this segment
                    let mut end = e;
                    if let Some((ib, _)) = last_old {
                        end = min(end, ib)
                    }
                    // Note: could try to aggregate insertions, but not sure of the win.
                    // Use the mapper to insert the corresponding section of the tombstones rope
                    els.push(DeltaElement::Insert(tombstones.subseq(
                        Interval::new_closed_open(m.doc_index_to_subset(beg), m.doc_index_to_subset(end)))));
                    beg = end;
                }
            }
        }
        Delta { els: els, base_len: base_len }
    }

    /// Produce a summary of the delta. Everything outside the returned interval
    /// is unchanged, and the old contents of the interval are replaced by new
    /// contents of the returned length. Equations:
    ///
    /// `(iv, new_len) = self.summary()`
    ///
    /// `new_s = self.apply(s)`
    ///
    /// `new_s = simple_edit(iv, new_s.subseq(iv.start(), iv.start() + new_len), s.len()).apply(s)`
    pub fn summary(&self) -> (Interval, usize) {
        let mut els = self.els.as_slice();
        let mut iv_start = 0;
        if let Some((&DeltaElement::Copy(0, end), rest)) = els.split_first() {
            iv_start = end;
            els = rest;
        }
        let mut iv_end = self.base_len;
        if let Some((&DeltaElement::Copy(beg, end), init)) = els.split_last() {
            if end == iv_end {
                iv_end = beg;
                els = init;
            }
        }
        (Interval::new_closed_open(iv_start, iv_end), Delta::total_element_len(els))
    }

    /// Returns the length of the new document. In other words, the length of
    /// the transformed string after this Delta is applied.
    ///
    /// `d.apply(r).len() == d.new_document_len()`
    pub fn new_document_len(&self) -> usize {
        Delta::total_element_len(self.els.as_slice())
    }

    fn total_element_len(els: &[DeltaElement<N>]) -> usize {
        els.iter().fold(0, |sum, el|
            sum + match *el {
                DeltaElement::Copy(beg, end) => end - beg,
                DeltaElement::Insert(ref n) => n.len()
            }
        )
    }
}

impl<N: NodeInfo> fmt::Debug for Delta<N> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "Delta("));
        for el in &self.els {
            match *el {
                DeltaElement::Copy(beg,end) => {
                    try!(write!(f, "[{},{}) ", beg, end));
                }
                DeltaElement::Insert(ref node) => {
                    try!(write!(f, "<ins:{}> ", node.len()));
                }
            }
        }
        try!(write!(f, ")"));
        Ok(())
    }
}

impl<N: NodeInfo> InsertDelta<N> {
    /// Do a coordinate transformation on an insert-only delta. The `after` parameter
    /// controls whether the insertions in `self` come after those specific in the
    /// coordinate transform.
    //
    // TODO: write accurate equations
    // TODO: can we infer l from the other inputs?
    pub fn transform_expand(&self, xform: &Subset, l: usize, after: bool) -> InsertDelta<N> {
        let cur_els = &self.0.els;
        let mut els = Vec::new();
        let mut x = 0;  // coordinate within self
        let mut y = 0;  // coordinate within xform
        let mut i = 0;  // index into self.els
        let mut b1 = 0;
        let mut xform_ranges = xform.complement_iter(l);
        let mut last_xform = xform_ranges.next();
        while y < l || i < cur_els.len() {
            let next_iv_beg = if let Some((xb, _)) = last_xform { xb } else { l };
            if after && y < next_iv_beg {
                y = next_iv_beg;
            }
            while i < cur_els.len() {
                match cur_els[i] {
                    DeltaElement::Insert(ref n) => {
                        if y > b1 {
                            els.push(DeltaElement::Copy(b1, y));
                        }
                        b1 = y;
                        els.push(DeltaElement::Insert(n.clone()));
                        i += 1;
                    }
                    DeltaElement::Copy(_b, e) => {
                        if y >= next_iv_beg {
                            let mut next_y = e + y - x;
                            if let Some((_, xe)) = last_xform {
                                next_y = min(next_y, xe);
                            }
                            x += next_y - y;
                            y = next_y;
                            if x == e {
                                i += 1;
                            }
                            if let Some((_, xe)) = last_xform {
                                if y == xe {
                                    last_xform = xform_ranges.next();
                                }
                            }
                        }
                        break;
                    }
                }
            }
            if !after && y < next_iv_beg {
                y = next_iv_beg;
            }
        }
        if y > b1 {
            els.push(DeltaElement::Copy(b1, y));
        }
        InsertDelta(Delta { els: els, base_len: l })
    }

    // TODO: it is plausible this method also works on Deltas with deletes
    /// Shrink a delta through a deletion of some of its copied regions with
    /// the same base. For example, if `self` applies to a union string, and
    /// `xform` is the deletions from that union, the resulting Delta will
    /// apply to the text.
    ///
    /// **Note:** this is similar to `Subset::transform_shrink` but *the argument
    /// order is reversed* due to this being a method on `InsertDelta`.
    pub fn transform_shrink(&self, xform: &Subset) -> InsertDelta<N> {
        let compl = xform.complement(self.base_len);
        let mut m = compl.mapper();
        let els = self.0.els.iter().map(|elem| {
            match *elem {
                DeltaElement::Copy(b, e) => {
                    DeltaElement::Copy(m.doc_index_to_subset(b), m.doc_index_to_subset(e))
                }
                DeltaElement::Insert(ref n) => {
                    DeltaElement::Insert(n.clone())
                }
            }
        }).collect();
        InsertDelta(Delta { els: els, base_len: xform.len_after_delete(self.base_len)})
    }

    /// Return a Subset containing the inserted ranges.
    ///
    /// `d.inserted_subset().delete_from_string(d.apply_to_string(s)) == s`
    pub fn inserted_subset(&self) -> Subset {
        let mut sb = SubsetBuilder::new();
        let mut x = 0;
        for elem in &self.0.els {
            match *elem {
                DeltaElement::Copy(b, e) => {
                    x += e - b;
                }
                DeltaElement::Insert(ref n) => {
                    sb.add_range(x, x + n.len());
                    x += n.len();
                }
            }
        }
        sb.build()
    }
}

/// An InsertDelta is a certain kind of Delta, and anything that applies to a
/// Delta that may include deletes also applies to one that definitely
/// doesn't. This impl allows implicit use of those methods.
impl<N: NodeInfo> Deref for InsertDelta<N> {
    type Target = Delta<N>;

    fn deref(&self) -> &Delta<N> {
        &self.0
    }
}

/// A mapping from coordinates in the source sequence to coordinates in the sequence after
/// the delta is applied.

// TODO: this doesn't need the new strings, so it should either be based on a new structure
// like Delta but missing the strings, or perhaps the two subsets it's synthesized from.
pub struct Transformer<'a, N: NodeInfo + 'a> {
    delta: &'a Delta<N>,
}

impl<'a, N: NodeInfo + 'a> Transformer<'a, N> {
    /// Create a new transformer from a delta.
    pub fn new(delta: &'a Delta<N>) -> Self {
        Transformer {
            delta: delta,
        }
    }

    /// Transform a single coordinate. The `after` parameter indicates whether it
    /// it should land before or after an inserted region.

    // TODO: implement a cursor so we're not scanning from the beginning every time.
    pub fn transform(&mut self, ix: usize, after: bool) -> usize {
        if ix == 0 && !after {
            return 0;
        }
        let mut result = 0;
        for el in &self.delta.els {
            match *el {
                DeltaElement::Copy(beg, end) => {
                    if ix <= beg {
                        return result;
                    }
                    if ix < end || (ix == end && !after) {
                        return result + ix - beg;
                    }
                    result += end - beg;
                }
                DeltaElement::Insert(ref n) => {
                    result += n.len();
                }
            }
        }
        return result;
    }

    /// Determine whether a given interval is untouched by the transformation.
    pub fn interval_untouched(&mut self, iv: Interval) -> bool {
        let mut last_was_ins = true;
        for el in &self.delta.els {
            match *el {
                DeltaElement::Copy(beg, end) => {
                    if iv.is_before(end) {
                        if last_was_ins {
                            if iv.is_after(beg) {
                                return true;
                            }
                        } else {
                            if !iv.is_before(beg) {
                                return true;
                            }
                        }
                    } else {
                        return false;
                    }
                    last_was_ins = false;
                }
                _ => {
                    last_was_ins = true;
                }
            }
        }
        false
    }
}

/// A builder for creating new `Delta` objects.
///
/// Note that all edit operations must be sorted; the start point of each
/// interval must be no less than the end point of the previous one.
pub struct Builder<N: NodeInfo> {
    delta: Delta<N>,
    last_offset: usize,
}

impl<N: NodeInfo> Builder<N> {
    /// Creates a new builder, applicable to a base rope of length `base_len`.
    pub fn new(base_len: usize) -> Builder<N> {
        Builder {
            delta: Delta {
                els: Vec::new(),
                base_len: base_len,
            },
            last_offset: 0,
        }
    }

    /// Deletes the given interval. Panics if interval is not properly sorted.
    pub fn delete(&mut self, interval: Interval) {
        let (start, end) = interval.start_end();
        assert!(start >= self.last_offset, "Delta builder: intervals not properly sorted");
        if start > self.last_offset {
            self.delta.els.push(DeltaElement::Copy(self.last_offset, start));
        }
        self.last_offset = end;
    }

    /// Replaces the given interval with the new rope. Panics if interval
    /// is not properly sorted.
    pub fn replace(&mut self, interval: Interval, rope: Node<N>) {
        self.delete(interval);
        self.delta.els.push(DeltaElement::Insert(rope));
    }

    /// Determines if delta would be a no-op transformation if built.
    pub fn is_empty(&self) -> bool {
        self.last_offset == 0 && self.delta.els.is_empty()
    }

    /// Builds the `Delta`.
    pub fn build(mut self) -> Delta<N> {
        if self.last_offset < self.delta.base_len {
            self.delta.els.push(DeltaElement::Copy(self.last_offset, self.delta.base_len));
        }
        self.delta
    }
}

#[cfg(test)]
mod tests {
    use rope::Rope;
    use delta::{Delta};
    use interval::Interval;
    use test_helpers::{find_deletions};

    const TEST_STR: &'static str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    #[test]
    fn simple() {
        let d = Delta::simple_edit(Interval::new_closed_open(1, 9), Rope::from("era"), 11);
        assert_eq!("herald", d.apply_to_string("hello world"));
        assert_eq!(6, d.new_document_len());
    }

    #[test]
    fn factor() {
        let d = Delta::simple_edit(Interval::new_closed_open(1, 9), Rope::from("era"), 11);
        let (d1, ss) = d.factor();
        assert_eq!("heraello world", d1.apply_to_string("hello world"));
        assert_eq!("hld", ss.delete_from_string("hello world"));
    }

    #[test]
    fn synthesize() {
        let d = Delta::simple_edit(Interval::new_closed_open(1, 9), Rope::from("era"), 11);
        let (d1, del) = d.factor();
        let ins = d1.inserted_subset();
        let del = del.transform_expand(&ins);
        let union_str = d1.apply_to_string("hello world");
        let union_len = union_str.len();
        let tombstones = ins.complement(union_len).delete_from_string(&union_str);
        let new_d = Delta::synthesize(&Rope::from(&tombstones), &ins, union_len, &ins, &del);
        assert_eq!("herald", new_d.apply_to_string("hello world"));
        let text = del.complement(union_len).delete_from_string(&union_str);
        let inv_d = Delta::synthesize(&Rope::from(&text), &del, union_len, &del, &ins);
        assert_eq!("hello world", inv_d.apply_to_string("herald"));
    }

    #[test]
    fn inserted_subset() {
        let d = Delta::simple_edit(Interval::new_closed_open(1, 9), Rope::from("era"), 11);
        let (d1, _ss) = d.factor();
        assert_eq!("hello world", d1.inserted_subset().delete_from_string("heraello world"));
    }

    #[test]
    fn transform_expand() {
        let str1 = "01259DGJKNQTUVWXYcdefghkmopqrstvwxy";
        let s1 = find_deletions(str1, TEST_STR);
        let d = Delta::simple_edit(Interval::new_closed_open(10, 12), Rope::from("+"), str1.len());
        assert_eq!("01259DGJKN+UVWXYcdefghkmopqrstvwxy", d.apply_to_string(str1));
        let (d2, _ss) = d.factor();
        assert_eq!("01259DGJKN+QTUVWXYcdefghkmopqrstvwxy", d2.apply_to_string(str1));
        let d3 = d2.transform_expand(&s1, TEST_STR.len(), false);
        assert_eq!("0123456789ABCDEFGHIJKLMN+OPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", d3.apply_to_string(TEST_STR));
        let d4 = d2.transform_expand(&s1, TEST_STR.len(), true);
        assert_eq!("0123456789ABCDEFGHIJKLMNOP+QRSTUVWXYZabcdefghijklmnopqrstuvwxyz", d4.apply_to_string(TEST_STR));
    }

    #[test]
    fn transform_shrink() {
        let d = Delta::simple_edit(Interval::new_closed_open(10, 12), Rope::from("+"), TEST_STR.len());
        let (d2, _ss) = d.factor();
        assert_eq!("0123456789+ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", d2.apply_to_string(TEST_STR));

        let str1 = "0345678BCxyz";
        let s1 = find_deletions(str1, TEST_STR);
        let d3 = d2.transform_shrink(&s1);
        assert_eq!("0345678+BCxyz", d3.apply_to_string(str1));

        let str2 = "356789ABCx";
        let s2 = find_deletions(str2, TEST_STR);
        let d4 = d2.transform_shrink(&s2);
        assert_eq!("356789+ABCx", d4.apply_to_string(str2));
    }
}
