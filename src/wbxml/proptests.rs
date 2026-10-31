//! Property tests: encoding round-trips and decoding never panics.

use proptest::prelude::*;

use super::{Element, Node, Tag, cp, decode, encode, to_xml};

/// A tag from a known code page.
fn tag() -> impl Strategy<Value = Tag> {
    (0..cp::PAGES.len()).prop_flat_map(|page| {
        let page = &cp::PAGES[page];
        (0..page.tags.len()).prop_map(move |i| Tag {
            page: page.number,
            token: page.tags[i].0,
        })
    })
}

/// Element trees shaped like EAS: an element holds either one text, one
/// opaque blob or child elements.
fn element() -> impl Strategy<Value = Element> {
    let leaf = prop_oneof![
        tag().prop_map(Element::new),
        (tag(), "[^\u{0}]{1,40}").prop_map(|(t, s)| Element::with_text(t, s)),
        (tag(), proptest::collection::vec(any::<u8>(), 1..40))
            .prop_map(|(t, d)| Element::with_opaque(t, d)),
    ];
    leaf.prop_recursive(5, 64, 6, |inner| {
        (tag(), proptest::collection::vec(inner, 1..6)).prop_map(|(t, children)| Element {
            tag: t,
            children: children.into_iter().map(Node::Element).collect(),
        })
    })
}

proptest! {
    #[test]
    fn encoding_round_trips(root in element()) {
        let bytes = encode(&root);
        let back = decode(&bytes).expect("own output decodes");
        prop_assert_eq!(back, root);
    }

    #[test]
    fn decoding_garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(root) = decode(&bytes) {
            let _ = to_xml(&root);
            let _ = encode(&root);
        }
    }

    #[test]
    fn decoding_damaged_messages_never_panics(
        root in element(),
        flips in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
    ) {
        let mut bytes = encode(&root);
        for (at, value) in flips {
            let i = at.index(bytes.len());
            bytes[i] = value;
        }
        let _ = decode(&bytes);
    }
}
