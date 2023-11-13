//
// file_path.rs
//
// Copyright (C) 2023 Posit Software, PBC. All rights reserved.
//
//

use std::env::current_dir;
use std::path::PathBuf;

use anyhow::Result;
use harp::object::RObject;
use harp::string::r_string_decode;
use harp::utils::r_normalize_path;
use regex::Regex;
use stdext::join;
use stdext::unwrap;
use stdext::IntoResult;
use tower_lsp::lsp_types::CompletionItem;

use crate::lsp::completions::completion_item::completion_item_from_direntry;
use crate::lsp::document_context::DocumentContext;

pub fn completions_from_file_path(
    context: &DocumentContext,
) -> Result<Option<Vec<CompletionItem>>> {
    log::info!("completions_from_file_path()");

    let node = context.node;

    if node.kind() != "string" {
        return Ok(None);
    }

    let mut completions: Vec<CompletionItem> = vec![];

    // Get the contents of the string token.
    //
    // NOTE: This includes the quotation characters on the string, and so
    // also includes any internal escapes! We need to decode the R string
    // before searching the path entries.
    let token = context.node.utf8_text(context.source.as_bytes())?;
    let contents = unsafe { r_string_decode(token).into_result()? };
    log::info!("String value (decoded): {}", contents);

    // Use R to normalize the path.
    let path = r_normalize_path(RObject::from(contents))?;

    // parse the file path and get the directory component
    let mut path = PathBuf::from(path.as_str());
    log::info!("Normalized path: {}", path.display());

    // if this path doesn't have a root, add it on
    if !path.has_root() {
        let root = current_dir()?;
        path = root.join(path);
    }

    // if this isn't a directory, get the parent path
    if !path.is_dir() {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }

    // look for files in this directory
    log::info!("Reading directory: {}", path.display());
    let entries = std::fs::read_dir(path)?;
    for entry in entries.into_iter() {
        let entry = unwrap!(entry, Err(error) => {
            log::error!("{}", error);
            continue;
        });

        let item = unwrap!(completion_item_from_direntry(entry), Err(error) => {
            log::error!("{}", error);
            continue;
        });

        completions.push(item);
    }

    // Push path completions starting with non-word characters to the bottom of
    // the sort list (like those starting with `.`)
    let pattern = Regex::new(r"^\w").unwrap();
    for item in &mut completions {
        if pattern.is_match(&item.label) {
            item.sort_text = Some(join!["1-", item.label]);
        } else {
            item.sort_text = Some(join!["2-", item.label]);
        }
    }

    Ok(Some(completions))
}
