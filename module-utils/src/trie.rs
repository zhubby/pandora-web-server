// Copyright 2024 Wladimir Palant
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This implements a specialized prefix tree (trie) data structure. The design goal are:
//!
//! * Memory-efficient data storage after the setup phase
//! * Zero allocation and copying during lookup
//! * Efficient lookup
//! * The labels are segmented with a separator character (forward slash) and only full segment
//!   matches are accepted.

use std::ops::Range;

/// Character to separate labels
pub(crate) const SEPARATOR: u8 = b'/';

/// Calculates the length of the longest common prefix of two labels. A common prefix is identical
/// and ends at a boundary in both labels (either end of the label or a separator character).
fn common_prefix_length(a: &[u8], b: &[u8]) -> usize {
    let mut length = 0;
    for i in 0..std::cmp::min(a.len(), b.len()) {
        if a[i] != b[i] {
            return length;
        }

        if a[i] == SEPARATOR {
            length = i;
        }
    }

    if a.len() == b.len() || (a.len() < b.len() && b[a.len()] == SEPARATOR) {
        // exact match or A is a prefix of B
        length = a.len();
    } else if a.len() > b.len() && a[b.len()] == SEPARATOR {
        // B is a prefix of A
        length = b.len();
    }
    length
}

/// A trie data structure
///
/// To use memory more efficiently and to improve locality, this stores all data in three vectors.
/// One lists all nodes, ordered in such a way that children of one node are always stored
/// consecutively and sorted by their label. A node stores an index range referring to its
/// children.
///
/// Since values are optional and potentially rather large, existing values are stored in a
/// separate vector. The node stores an optional index of its value, not the value itself.
///
/// Finally, the third vector stores the labels of the nodes, so that nodes don’t need separate
/// allocations for their labels. Each nodes refers to its label within this vector via an index
/// range.
#[derive(Debug)]
pub(crate) struct Trie<Value> {
    nodes: Vec<Node>,
    values: Vec<Value>,
    labels: Vec<u8>,
}

/// A trie node
///
/// A node label can consist of one or multiple segments (separated by `SEPARATOR`). These segments
/// represent the route to the node from its parent node.
///
/// The value is optional. Nodes without a value serve merely as a routing point for multiple child
/// nodes.
///
/// Each child node represents a unique path further from this node. Multiple child node labels
/// never start with the same segment: in such scenarios the builder inserts an intermediate node
/// that serves as the common parent for all nodes reachable via that segment.
#[derive(Debug)]
struct Node {
    label: Range<usize>,
    value: Option<usize>,
    children: Range<usize>,
}

impl<Value> Trie<Value> {
    /// Index of the root node in the `nodes` vector, this is where lookup always starts.
    const ROOT: usize = 0;

    /// Returns a builder instance that can be used to set up the trie.
    pub(crate) fn builder() -> TrieBuilder<Value> {
        TrieBuilder::<Value>::new()
    }

    /// Looks up a particular label in the trie.
    ///
    /// The label is identified by an iterator producing segments. The segments are expected to be
    /// normalized: no empty segments exist and no segments contain the separator character.
    ///
    /// This will return the value corresponding to the longest matching path if any. In addition,
    /// the result contains the number of segments consumed.
    pub(crate) fn lookup<'a, 'b, L>(&'a self, mut label: L) -> Option<(&'a Value, usize)>
    where
        L: Iterator<Item = &'b [u8]>,
    {
        let mut result = None;
        let mut current = self.nodes.get(Self::ROOT)?;
        let mut current_segment = 0;
        loop {
            if let Some(value) = current.value {
                result = Some((self.values.get(value)?, current_segment));
            }

            let segment = if let Some(segment) = label.next() {
                current_segment += 1;
                segment
            } else {
                // End of label, return whatever we’ve got
                return result;
            };

            // TODO: Binary search might be more efficient here
            let mut found_match = false;
            for child in current.children.start..current.children.end {
                let child = self.nodes.get(child)?;
                let mut label_start = child.label.start;
                let label_end = child.label.end;
                let length = common_prefix_length(segment, &self.labels[label_start..label_end]);
                if length > 0 {
                    label_start += length;

                    // Keep matching more segments until there is no more label left
                    while label_end > label_start {
                        // Skip separator character
                        label_start += 1;

                        let segment = if let Some(segment) = label.next() {
                            current_segment += 1;
                            segment
                        } else {
                            // End of label, return whatever we’ve got
                            return result;
                        };

                        let length =
                            common_prefix_length(segment, &self.labels[label_start..label_end]);
                        if length > 0 {
                            label_start += length;
                        } else {
                            // Got only a partial match
                            return result;
                        }
                    }

                    found_match = true;
                    current = child;
                    break;
                }
            }

            if !found_match {
                return result;
            }
        }
    }
}

/// A trie builder used to set up a `Trie` instance
///
/// In addition to setting up the trie structure, this will keep track of the requires allocation
/// size for the trie vectors.
#[derive(Debug)]
pub(crate) struct TrieBuilder<Value> {
    nodes: usize,
    labels: usize,
    values: usize,
    root: BuilderNode<Value>,
}

/// A builder node
///
/// Unlike `Node` this data structure references its label, children and value directly.
#[derive(Debug)]
struct BuilderNode<Value> {
    label: Vec<u8>,
    children: Vec<BuilderNode<Value>>,
    value: Option<Value>,
}

impl<Value> TrieBuilder<Value> {
    /// Creates a new builder.
    fn new() -> Self {
        Self {
            nodes: 1,
            labels: 0,
            values: 0,
            root: BuilderNode::<Value> {
                label: Vec::new(),
                children: Vec::new(),
                value: None,
            },
        }
    }

    /// Recursively finds the node that a particular label should be added to.
    ///
    /// If the label shares a common prefix with a child node of the current node, this will
    /// insert a new intermediate node if necessary (new parent for both than child node and the
    /// node to be added) and recurses. As it recurses, it will trim down the label accordingly to
    /// the path already traveled.
    ///
    /// If no nodes with common prefixes are found, then the current node is the one that the
    /// label should be added to.
    fn find_insertion_point<'a>(
        current: &'a mut BuilderNode<Value>,
        nodes: &mut usize,
        labels: &mut usize,
        label: &mut Vec<u8>,
    ) -> &'a mut BuilderNode<Value> {
        let mut match_ = None;
        for (i, node) in current.children.iter_mut().enumerate() {
            let length = common_prefix_length(&node.label, label);
            if length > 0 {
                label.drain(..std::cmp::min(length + 1, label.len()));
                if length < node.label.len() {
                    // Partial match, insert a new node and make the original its child
                    let mut head: Vec<_> = node.label.drain(..length + 1).collect();

                    // Remove separator
                    head.pop();

                    *nodes += 1;

                    // Splitting the node label in two results in one character less (separator)
                    *labels -= 1;

                    let mut new_node = BuilderNode {
                        label: head,
                        children: Vec::new(),
                        value: None,
                    };

                    std::mem::swap(node, &mut new_node);
                    node.children.push(new_node);
                };

                match_ = Some(i);
                break;
            }
        }

        return match match_ {
            Some(i) => Self::find_insertion_point(&mut current.children[i], nodes, labels, label),
            None => current,
        };
    }

    /// Adds a value for the given label. Will return `true` if an existing value was overwritten.
    ///
    /// The label is expected to be normalized: no separator characters at the beginning or end, and
    /// always only one separator character used to separate segments.
    pub(crate) fn push(&mut self, mut label: Vec<u8>, value: Value) -> bool {
        let node = Self::find_insertion_point(
            &mut self.root,
            &mut self.nodes,
            &mut self.labels,
            &mut label,
        );

        if label.is_empty() {
            // Exact match, replace the value for this node
            let had_value = node.value.is_some();
            if !had_value {
                self.values += 1;
            }
            node.value = Some(value);
            had_value
        } else {
            // Insert new node as child of the current one
            self.nodes += 1;
            self.values += 1;
            self.labels += label.len();
            node.children.push(BuilderNode {
                label,
                children: Vec::new(),
                value: Some(value),
            });
            false
        }
    }

    /// Pushes an empty entry into the nodes vector.
    ///
    /// This is used to allocate space for the node, so that child nodes are always stored
    /// consecutively. The values are adjusted by `into_trie_node` later.
    fn push_trie_node(nodes: &mut Vec<Node>) {
        nodes.push(Node {
            label: 0..0,
            value: None,
            children: 0..0,
        });
    }

    /// Sets up an entry in the nodes vector.
    ///
    /// This will transfer data from a builder node to the trie node identified via index. It will
    /// also recurse to make sure child nodes of the current node are transferred as well.
    fn into_trie_node(
        mut current: BuilderNode<Value>,
        index: usize,
        nodes: &mut Vec<Node>,
        labels: &mut Vec<u8>,
        values: &mut Vec<Value>,
    ) {
        nodes[index].label = labels.len()..labels.len() + current.label.len();
        labels.append(&mut current.label);

        if let Some(value) = current.value {
            nodes[index].value = Some(values.len());
            values.push(value);
        }

        current.children.sort_by(|a, b| a.label.cmp(&b.label));

        let mut child_index = nodes.len();
        nodes[index].children = child_index..child_index + current.children.len();
        for _ in &current.children {
            Self::push_trie_node(nodes);
        }

        for child in current.children {
            Self::into_trie_node(child, child_index, nodes, labels, values);
            child_index += 1;
        }
    }

    /// Translates the builder data into a `Trie` instance.
    pub(crate) fn build(self) -> Trie<Value> {
        let mut nodes = Vec::with_capacity(self.nodes);
        let mut labels = Vec::with_capacity(self.labels);
        let mut values = Vec::with_capacity(self.values);

        let index = nodes.len();
        Self::push_trie_node(&mut nodes);
        Self::into_trie_node(self.root, index, &mut nodes, &mut labels, &mut values);

        assert_eq!(nodes.len(), self.nodes);
        assert_eq!(labels.len(), self.labels);
        assert_eq!(values.len(), self.values);

        Trie {
            nodes,
            labels,
            values,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key<'a>(s: &'a str) -> Box<dyn Iterator<Item = &[u8]> + 'a> {
        Box::new(
            s.as_bytes()
                .split(|c| *c == SEPARATOR)
                .filter(|s| !s.is_empty()),
        )
    }

    #[test]
    fn common_prefix() {
        assert_eq!(common_prefix_length(b"", b""), 0);
        assert_eq!(common_prefix_length(b"abc", b""), 0);
        assert_eq!(common_prefix_length(b"", b"abc"), 0);
        assert_eq!(common_prefix_length(b"abc", b"abc"), 3);
        assert_eq!(common_prefix_length(b"a", b"abc"), 0);
        assert_eq!(common_prefix_length(b"abc", b"a"), 0);
        assert_eq!(common_prefix_length(b"a", b"a/bc"), 1);
        assert_eq!(common_prefix_length(b"a/bc", b"a"), 1);
        assert_eq!(common_prefix_length(b"a/b", b"a/bc"), 1);
        assert_eq!(common_prefix_length(b"a/bc", b"a/b"), 1);
        assert_eq!(common_prefix_length(b"a/bc", b"a/bc"), 4);
        assert_eq!(common_prefix_length(b"a/bc", b"a/bc/d"), 4);
        assert_eq!(common_prefix_length(b"a/bc/d", b"a/bc"), 4);
        assert_eq!(common_prefix_length(b"a/bc/d", b"x/bc/d"), 0);
    }

    #[test]
    fn lookup_with_root_value() {
        let mut builder = Trie::builder();
        for (label, value) in [
            ("", 1),
            ("a", 2),
            ("bc", 7),
            ("a/bc/de/f", 3),
            ("a/bc", 4),
            ("a/bc/de/g", 5),
        ] {
            assert!(!builder.push(label.as_bytes().to_vec(), value));
        }
        assert!(builder.push("a/bc".as_bytes().to_vec(), 6));
        let trie = builder.build();

        assert_eq!(trie.lookup(make_key("")), Some((&1, 0)));
        assert_eq!(trie.lookup(make_key("a")), Some((&2, 1)));
        assert_eq!(trie.lookup(make_key("x")), Some((&1, 0)));
        assert_eq!(trie.lookup(make_key("bc")), Some((&7, 1)));
        assert_eq!(trie.lookup(make_key("x/y")), Some((&1, 0)));
        assert_eq!(trie.lookup(make_key("a/bc")), Some((&6, 2)));
        assert_eq!(trie.lookup(make_key("a/b")), Some((&2, 1)));
        assert_eq!(trie.lookup(make_key("a/bcde")), Some((&2, 1)));
        assert_eq!(trie.lookup(make_key("a/bc/de")), Some((&6, 2)));
        assert_eq!(trie.lookup(make_key("a/bc/de/f")), Some((&3, 4)));
        assert_eq!(trie.lookup(make_key("a/bc/de/fh")), Some((&6, 2)));
        assert_eq!(trie.lookup(make_key("a/bc/de/g")), Some((&5, 4)));
        assert_eq!(trie.lookup(make_key("a/bc/de/h")), Some((&6, 2)));
    }

    #[test]
    fn lookup_without_root_value() {
        let mut builder = Trie::builder();
        for (label, value) in [
            ("a", 2),
            ("bc", 7),
            ("a/bc/de/f", 3),
            ("a/bc", 4),
            ("a/bc/de/g", 5),
        ] {
            assert!(!builder.push(label.as_bytes().to_vec(), value));
        }
        assert!(builder.push("a/bc".as_bytes().to_vec(), 6));
        let trie = builder.build();

        assert_eq!(trie.lookup(make_key("")), None);
        assert_eq!(trie.lookup(make_key("a")), Some((&2, 1)));
        assert_eq!(trie.lookup(make_key("x")), None);
        assert_eq!(trie.lookup(make_key("b")), None);
        assert_eq!(trie.lookup(make_key("bc")), Some((&7, 1)));
        assert_eq!(trie.lookup(make_key("bcd")), None);
        assert_eq!(trie.lookup(make_key("x/y")), None);
        assert_eq!(trie.lookup(make_key("a/bc")), Some((&6, 2)));
        assert_eq!(trie.lookup(make_key("a/b")), Some((&2, 1)));
        assert_eq!(trie.lookup(make_key("a/bcde")), Some((&2, 1)));
        assert_eq!(trie.lookup(make_key("a/bc/de")), Some((&6, 2)));
        assert_eq!(trie.lookup(make_key("a/bc/de/f")), Some((&3, 4)));
        assert_eq!(trie.lookup(make_key("a/bc/de/fh")), Some((&6, 2)));
        assert_eq!(trie.lookup(make_key("a/bc/de/g")), Some((&5, 4)));
        assert_eq!(trie.lookup(make_key("a/bc/de/h")), Some((&6, 2)));
    }
}
