//! Tests of [`image_references`] on images written by hand in D-1188's
//! format.

use crate::image_references;

/// An image header: `CHBV`, format 1, the path, the object count.
fn header(path: &str, count: u8) -> Vec<u8> {
    let mut out = b"CHBV".to_vec();
    out.push(1);
    out.push(u8::try_from(path.len()).unwrap());
    out.extend_from_slice(path.as_bytes());
    out.push(count);
    out
}

/// A tag-3 reference to object `index` of the image of `path`.
fn extern_ref(path: &str, index: u8) -> Vec<u8> {
    let mut out = vec![3, u8::try_from(path.len()).unwrap()];
    out.extend_from_slice(path.as_bytes());
    out.push(index);
    out
}

#[test]
fn const_cache_image_references_lists_earlier_images() {
    // `app::b = (app::a's list, Some(3), "x", [app::c's object])`: objects
    // 0 a string, 1 a list holding a tag-3 reference, 2 the tuple.
    let mut bytes = header("app::b", 3);
    bytes.extend_from_slice(&[1, 2]); // root: object 2
    bytes.extend_from_slice(&[1, 1, b'x']); // 0: "x"
    bytes.extend_from_slice(&[12, 1]); // 1: a list of one element
    bytes.extend(extern_ref("app::c", 4));
    bytes.extend_from_slice(&[7, 4]); // 2: a tuple of four
    bytes.extend(extern_ref("app::a", 0));
    bytes.extend_from_slice(&[2, 8, 3, 0]); // the static `Some(3)`
    bytes.extend_from_slice(&[1, 0]);
    bytes.extend_from_slice(&[1, 1]);
    assert_eq!(
        image_references(&bytes),
        Some(vec!["app::a".into(), "app::c".into()])
    );
}

#[test]
fn const_cache_image_references_reads_maps_and_constructors() {
    // A map of one entry whose value is a user constructor with a raw
    // `Int` field and a uniform field referencing `app::a`.
    let mut bytes = header("app::m", 2);
    bytes.extend_from_slice(&[1, 1]); // root: object 1
    bytes.push(11);
    bytes.extend_from_slice(&7_u64.to_le_bytes()); // type key
    bytes.extend_from_slice(&[0, 2]); // constructor 0, two fields
    bytes.push(1); // `Int`, raw
    bytes.extend_from_slice(&5_u64.to_le_bytes());
    bytes.push(0); // `Value`, a reference
    bytes.extend(extern_ref("app::a", 2));
    bytes.extend_from_slice(&[13, 1]); // a map of one entry
    bytes.push(0);
    bytes.extend_from_slice(&3_u64.to_le_bytes()); // key: an immediate
    bytes.extend_from_slice(&[1, 0]); // value: object 0
    assert_eq!(image_references(&bytes), Some(vec!["app::a".into()]));
}

#[test]
fn const_cache_image_references_refuses_what_it_cannot_read() {
    let mut plain = header("app::n", 0);
    plain.extend_from_slice(&[0]);
    plain.extend_from_slice(&3_u64.to_le_bytes());
    assert_eq!(image_references(&plain), Some(Vec::new()));
    // Trailing bytes, a truncated image, another format and an address.
    let mut trailing = plain.clone();
    trailing.push(0);
    assert_eq!(image_references(&trailing), None);
    assert_eq!(
        image_references(plain.get(..plain.len() - 1).unwrap()),
        None
    );
    let mut other = plain.clone();
    other[4] = 2;
    assert_eq!(image_references(&other), None);
    let mut address = header("app::n", 0);
    address.push(4);
    address.extend_from_slice(&64_u64.to_le_bytes());
    assert_eq!(image_references(&address), None);
}
