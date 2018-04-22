extern crate little_skeptic;

fn main() {
    little_skeptic::generate_doc_tests(&[
        "README.md",
        "template-example.md",
        "tests/macro-use.md",
        "tests/hashtag-test.md",
        "tests/should-panic-test.md",
        "tests/section-names.md",
    ]);
}
