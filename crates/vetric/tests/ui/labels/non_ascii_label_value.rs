use vetric::EncodeLabelValue;

#[derive(Debug, EncodeLabelValue)]
#[metrics(rename_all = "snake_case")]
enum Label {
    Хорошо,
}

fn main() {}
