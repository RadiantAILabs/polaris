use polaris_tools::tool;

#[tool(unknown = true)]
/// An unsupported `#[tool(...)]` option should be rejected.
async fn bad(name: String) -> Result<String, String> {
    Ok(name)
}

fn main() {}
