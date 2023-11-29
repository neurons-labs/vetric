use vetric::EncodeLabelValue;

#[derive(Debug, EncodeLabelValue)]
#[metrics(format = "{}")]
enum Label {
    Test,
    Value,
}

fn main() {}
