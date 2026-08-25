use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Behavioral validation for the installed CA bundle. The parser accepts the
// descriptive text curl puts between certificates, but strictly balances PEM
// boundaries and requires nonempty base64-shaped content in every block.
pub fn recipe() -> Recipe {
    let bundle = "{in:ca-certificates}/share/ca-certificates/ca-bundle.crt";
    let parser = r#"
function valid_body(line) {
    return line ~ /^[A-Za-z0-9+\/=]+$/ &&
        length(line) % 4 == 0 &&
        line !~ /=[^=]/ &&
        line !~ /===/
}
function accept_body(line) {
    if (saw_padding || !valid_body(line)) {
        return 0
    }
    if (line ~ /=/) {
        saw_padding = 1
    }
    return 1
}
BEGIN {
    inside = 0
    certificates = 0
    body_lines = 0
    if (valid_body("=") || valid_body("A") || valid_body("A=A=") ||
        valid_body("AAAA=AAA") || !valid_body("AAAA") ||
        !valid_body("AAA=") || !valid_body("AA==")) {
        print "broken PEM body validator" > "/dev/stderr"
        exit 1
    }
    saw_padding = 0
    if (!accept_body("AAA=") || accept_body("BBBB")) {
        print "broken PEM padding validator" > "/dev/stderr"
        exit 1
    }
    saw_padding = 0
}
$0 == "-----BEGIN CERTIFICATE-----" {
    if (inside) {
        print "nested PEM certificate boundary" > "/dev/stderr"
        exit 1
    }
    inside = 1
    body_lines = 0
    saw_padding = 0
    next
}
$0 == "-----END CERTIFICATE-----" {
    if (!inside || body_lines == 0) {
        print "empty or unmatched PEM certificate" > "/dev/stderr"
        exit 1
    }
    inside = 0
    certificates++
    next
}
inside {
    if (!accept_body($0)) {
        print "invalid PEM certificate body" > "/dev/stderr"
        exit 1
    }
    body_lines++
    next
}
$0 ~ /^-----/ {
    print "unmatched PEM boundary" > "/dev/stderr"
    exit 1
}
END {
    if (inside || certificates == 0) {
        print "CA bundle contains no complete PEM certificate" > "/dev/stderr"
        exit 1
    }
}
"#;
    let steps = vec![
        Step::run("{root}", &["{in:busybox-x86-64}/bin/awk", parser, bundle]),
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: installed CA bundle contains complete PEM certificates\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("ca-certificates-test", "1.0")
        .native_inputs(&["busybox-x86-64"])
        .inputs(&["ca-certificates"])
        .steps(steps)
        .checks(vec![
            RecipeCheck::new(
                r#"
echo ">> recipe-check ca-certificates-test: install curl's pinned Mozilla CA extract and validate a nonempty PEM certificate bundle"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run ca-certificates-test 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}
