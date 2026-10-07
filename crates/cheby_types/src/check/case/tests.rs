//! Tests of `case.rs`: subjects, arms, guards, arm types and the
//! subjectless form (§5.9).

use crate::test_support::{Build, build};
use crate::thir::ExprKind;

fn check(items: &str) -> Build {
    build(&[("", &format!("//! Test.\n\n{items}"))])
}

fn messages(items: &str) -> Vec<String> {
    check(items).messages()
}

#[test]
fn case_forms_check_into_thir() {
    let src = "fn both(a: Bool, b: Bool) -> String {\n  case a, b {\n    True, True => \"both\"\n    True, False => \"one\"\n    False, True => \"one\"\n    False, False => \"none\"\n  }\n}\n\n\
               fn sign(n: Int) -> String {\n  case {\n    n < 0 => \"negative\"\n    n == 0 => \"zero\"\n    _ => \"positive\"\n  }\n}\n\n\
               fn guarded(o: Option<Int>) -> Int {\n  case o {\n    Some(n) when n > 0 => n\n    Some(_) => 0\n    None => panic as \"none\"\n  }\n}\n";
    let b = check(src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());
    let case_of = |name: &str| {
        let body = b.thir_fn("app", name).unwrap().body.clone().unwrap();
        body.exprs
            .iter()
            .find_map(|e| match &e.kind {
                ExprKind::Case { subjects, arms } => Some((subjects.len(), arms.clone())),
                _ => None,
            })
            .unwrap()
    };
    let (subjects, arms) = case_of("both");
    assert_eq!(subjects, 2);
    assert!(arms.iter().all(|a| a.pats.len() == 2 && a.guard.is_none()));
    let (subjects, arms) = case_of("sign");
    assert_eq!(subjects, 0);
    assert_eq!(arms.len(), 3);
    assert!(arms[0].guard.is_some() && arms[0].pats.is_empty());
    assert!(arms[2].guard.is_none() && arms[2].pats.is_empty());
    let (_, arms) = case_of("guarded");
    assert!(arms[0].guard.is_some());
    assert!(arms[1].guard.is_none());
}

#[test]
fn each_arm_has_one_pattern_per_subject() {
    let src =
        "fn a(x: Bool, y: Bool) -> Int {\n  case x, y {\n    True => 1\n    _, _, _ => 2\n  }\n}\n";
    let got = messages(src);
    assert_eq!(
        got,
        [
            "E0048: this arm has 1 pattern, but the `case` has 2 subjects [\"expected 2 patterns\"]",
            "E0048: this arm has 3 patterns, but the `case` has 2 subjects [\"expected 2 patterns\"]",
        ]
    );
}

#[test]
fn guards_and_conditions_are_bool() {
    let src = "fn a(n: Int) -> Int {\n  case n {\n    x when x => 1\n    _ => 0\n  }\n}\n\n\
               fn b(n: Int) -> Int {\n  case {\n    n => 1\n    _ => 0\n  }\n}\n";
    let got = messages(src);
    assert_eq!(
        got,
        [
            "E0047: mismatched types [\"expected `Bool`, found `Int`\"]",
            "E0047: mismatched types [\"expected `Bool`, found `Int`\"]",
        ]
    );
}

#[test]
fn arms_have_one_type() {
    // With an expected type, each arm is checked against it.
    let src =
        "fn a(b: Bool) -> String {\n  case b {\n    True => \"yes\"\n    False => Nil\n  }\n}\n";
    assert_eq!(
        messages(src),
        [
            "E0047: mismatched types [\"expected `String`, found `Nil`\", \"expected because of this return type\"]"
        ]
    );
    // Without one, the first arm fixes the type.
    let src = "fn a(b: Bool) -> Nil {\n  let _ = case b {\n    True => \"yes\"\n    False => Nil\n  }\n}\n";
    assert_eq!(
        messages(src),
        [
            "E0047: mismatched types [\"expected `String`, found `Nil`\", \"the `case` has the type of this first arm\"]"
        ]
    );
}

#[test]
fn diverging_arms_take_the_other_arms_type() {
    // §3.12.5: the first arm that is not `Never` fixes the type, even
    // after a diverging arm.
    let src = "fn first(flag: Bool) -> String {\n  let x = case flag {\n    True => panic as \"no\"\n    False => \"yes\"\n  }\n  x\n}\n\n\
               fn second(flag: Bool) -> String {\n  let x = case flag {\n    True => \"yes\"\n    False => panic\n  }\n  x\n}\n\n\
               fn third(flag: Bool) -> String {\n  case flag {\n    True => panic\n    False => panic\n  }\n}\n";
    let b = check(src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());
}

#[test]
fn bindings_of_an_arm_are_usable_in_its_guard_and_body() {
    let src = "type Event {\n  Click { x: Int, y: Int },\n  Key { code: Int },\n}\n\n\
               fn describe(event: Event) -> String {\n  case event {\n    Click { x, .. } when x < 0 => \"off-screen\"\n    Click { .. } => \"click\"\n    Key { code: 27 } => \"escape\"\n    Key { .. } => \"key\"\n  }\n}\n";
    let b = check(src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());
}
