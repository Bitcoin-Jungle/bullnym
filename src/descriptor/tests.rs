use super::*;

const TEST_CT_DESC: &str = "ct(slip77(9c8e4f05c7711a98c838be228bcb84924d4570ca53f35fa1c793e58841d47023),elwpkh([73c5da0a/84h/1776h/0h]xpub6CRFzUgHFDaiDAQFNX7VeV9JNPDRabq6NYSpzVZ8zW8ANUCiDdenkb1gBoEZuXNZb3wPc1SVcDXgD2ww5UBtTb8s8ArAbTkoRQ8qn34KgcY/<0;1>/*))#y8jljyxl";

#[test]
fn derive_valid_address() {
    let addr = derive_address(TEST_CT_DESC, 0).expect("descriptor must parse");
    assert!(
        addr.starts_with("lq1qq"),
        "expected confidential address, got {addr}"
    );
}

#[test]
fn deterministic_derivation() {
    let a1 = derive_address(TEST_CT_DESC, 5).unwrap();
    let a2 = derive_address(TEST_CT_DESC, 5).unwrap();
    assert_eq!(a1, a2);
}

#[test]
fn different_indices_different_addresses() {
    let a0 = derive_address(TEST_CT_DESC, 0).unwrap();
    let a1 = derive_address(TEST_CT_DESC, 1).unwrap();
    let a2 = derive_address(TEST_CT_DESC, 2).unwrap();
    assert_ne!(a0, a1);
    assert_ne!(a1, a2);
    assert_ne!(a0, a2);
}

#[test]
fn invalid_descriptor_fails() {
    assert!(derive_address("not a descriptor", 0).is_err());
}

#[test]
fn empty_descriptor_fails() {
    assert!(derive_address("", 0).is_err());
}

#[test]
fn validate_too_long_descriptor() {
    assert!(validate_descriptor(TEST_CT_DESC, 10).is_err());
}

#[test]
fn validate_invalid_descriptor() {
    assert!(validate_descriptor("garbage", 1000).is_err());
}
