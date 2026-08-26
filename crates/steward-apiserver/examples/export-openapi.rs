use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;

use steward_apiserver::ApiDoc;
use utoipa::OpenApi;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let output = arguments
        .next()
        .ok_or("usage: export-openapi <output-file>")?;
    if arguments.next().is_some() {
        return Err("usage: export-openapi <output-file>".into());
    }

    let output = Path::new(&output);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut document = serde_json::to_string_pretty(&ApiDoc::openapi())?;
    document.push('\n');
    fs::write(output, document)?;
    Ok(())
}
