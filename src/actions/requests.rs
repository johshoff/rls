// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Requests that the RLS can respond to.

use actions::ActionContext;
use url::Url;
use vfs::FileContents;
use racer;
use rustfmt::{Input as FmtInput, format_input};
use rustfmt::file_lines::{Range as RustfmtRange, FileLines};
use serde_json;
use span;
use rayon;

use lsp_data;
use lsp_data::*;
use server::{Output, Ack, Action, RequestAction, LsState};
use jsonrpc_core::types::ErrorCode;

use std::collections::HashMap;
use std::time::{Duration};
use std::sync::{mpsc, Arc};

/// A request for information about a symbol in this workspace.
pub struct WorkspaceSymbol;

impl<'a> Action<'a> for WorkspaceSymbol {
    type Params = lsp_data::WorkspaceSymbolParams;
    const METHOD: &'static str = "workspace/symbol";

    fn new(_: &'a mut LsState) -> Self {
        WorkspaceSymbol
    }
}

impl<'a> RequestAction<'a> for WorkspaceSymbol {
    type Response = Vec<SymbolInformation>;

    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let analysis = ctx.analysis.clone();

        let receiver = receive_from_thread(move || {
            let defs = analysis.name_defs(&params.query).unwrap_or_else(|_| vec![]);

            defs.into_iter().map(|d| {
                SymbolInformation {
                    name: d.name,
                    kind:  source_kind_from_def_kind(d.kind),
                    location: ls_util::rls_to_location(&d.span),
                    container_name: d.parent.and_then(|id| analysis.get_def(id).ok()).map(|parent| parent.name)
                }
            }).collect()
        });

        Ok(receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT))
            .unwrap_or_else(|_| vec![]))
    }
}

/// A request for a flat list of all symbols found in a given text document.
pub struct Symbols;

impl<'a> Action<'a> for Symbols {
    type Params = DocumentSymbolParams;
    const METHOD: &'static str = "textDocument/documentSymbol";

    fn new(_: &'a mut LsState) -> Self {
        Symbols
    }
}

impl<'a> RequestAction<'a> for Symbols {
    type Response = Vec<SymbolInformation>;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "symbols")?;

        let analysis = ctx.analysis.clone();

        let receiver = receive_from_thread(move || {
            let symbols = analysis.symbols(&file_path).unwrap_or_else(|_| vec![]);

            symbols.into_iter().map(|s| {
                SymbolInformation {
                    name: s.name,
                    kind: source_kind_from_def_kind(s.kind),
                    location: ls_util::rls_to_location(&s.span),
                    container_name: None // FIXME: more info could be added here
                }
            }).collect()
        });

        Ok(receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT))
            .unwrap_or_else(|_| vec![]))
    }
}

/// Handles requests for hover information at a given point.
pub struct Hover;

impl<'a> Action<'a> for Hover {
    type Params = TextDocumentPositionParams;
    const METHOD: &'static str = "textDocument/hover";

    fn new(_: &'a mut LsState) -> Self {
        Hover
    }
}

impl<'a> RequestAction<'a> for Hover {
    type Response = lsp_data::Hover;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "hover")?;
        let span = ctx.convert_pos_to_span(file_path, params.position);

        trace!("hover: {:?}", span);

        let analysis = ctx.analysis.clone();
        let receiver = receive_from_thread(move || {
            let ty = analysis.show_type(&span).unwrap_or_else(|_| String::new());
            let docs = analysis.docs(&span).unwrap_or_else(|_| String::new());
            let doc_url = analysis.doc_url(&span).unwrap_or_else(|_| String::new());

            let mut contents = vec![];
            if !docs.is_empty() {
                contents.push(MarkedString::from_markdown(docs.into()));
            }
            if !doc_url.is_empty() {
                contents.push(MarkedString::from_markdown(doc_url.into()));
            }
            if !ty.is_empty() {
                contents.push(MarkedString::from_language_code("rust".into(), ty.into()));
            }
            lsp_data::Hover {
                contents: contents,
                range: None, // TODO: maybe add?
            }
        });

        Ok(receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT))
            .unwrap_or_else(|_| lsp_data::Hover {
                contents: vec![],
                range: None,
            }))
    }
}

/// Find all the implementations of a given trait.
pub struct FindImpls;

impl<'a> Action<'a> for FindImpls {
    type Params = TextDocumentPositionParams;
    const METHOD: &'static str = "rustDocument/implementations";

    fn new(_: &'a mut LsState) -> Self {
        FindImpls
    }
}

impl<'a> RequestAction<'a> for FindImpls {
    type Response = Vec<Location>;
    fn handle<O: Output>(&mut self, id: usize, params: Self::Params, ctx: &mut ActionContext, out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "find_impls")?;
        let span = ctx.convert_pos_to_span(file_path, params.position);
        let analysis = ctx.analysis.clone();

        let receiver = receive_from_thread(move || {
            let type_id = analysis.id(&span)?;
            let result = analysis.find_impls(type_id).map(|spans| {
                spans.into_iter().map(|x| ls_util::rls_to_location(&x)).collect()
            });
            result
        });

        let result = receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT));
        trace!("find_impls: {:?}", result);

        match result {
            Ok(Ok(r)) => Ok(r),
            _ => {
                out.failure_message(
                    id,
                    ErrorCode::InternalError,
                    "Find Implementations failed to complete successfully"
                );
                Err(())
            }
        }
    }
}

/// Get a list of definitions for item at the given point or identifier.
pub struct Definition;

impl<'a> Action<'a> for Definition {
    type Params = TextDocumentPositionParams;
    const METHOD: &'static str = "textDocument/definition";

    fn new(_: &'a mut LsState) -> Self {
        Definition
    }
}

impl<'a> RequestAction<'a> for Definition {
    type Response = Vec<Location>;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "goto_def")?;
        let span = ctx.convert_pos_to_span(file_path.clone(), params.position);
        let analysis = Arc::clone(&ctx.analysis);
        let vfs = Arc::clone(&ctx.vfs);
        let config = Arc::clone(&ctx.config);

        let receiver = receive_from_thread(move || {
            // If configured start racer concurrently and fallback to racer result
            let racer_receiver = {
                if config.lock().unwrap().goto_def_racer_fallback {
                    Some(receive_from_thread(move || {
                        let cache = racer::FileCache::new(vfs);
                        let session = racer::Session::new(&cache);
                        let location = pos_to_racer_location(params.position);

                        racer::find_definition(file_path, location, &session)
                            .and_then(location_from_racer_match)
                    }))
                }
                else { None }
            };

            match analysis.goto_def(&span) {
                Ok(out) => {
                    let result = vec![ls_util::rls_to_location(&out)];
                    trace!("goto_def (compiler): {:?}", result);
                    return result
                }
                _ => match racer_receiver {
                    Some(receiver) => match receiver.recv() {
                        Ok(Some(r)) =>  {
                            trace!("goto_def (Racer): {:?}", r);
                            return vec![r]
                        }
                        Ok(None) => {
                            trace!("goto_def (Racer): None");
                            return vec![]
                        }
                        _ => vec![]
                    }
                    _ => vec![]
                }
            }
        });

        Ok(receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT))
            .unwrap_or_else(|_| vec![]))
    }
}

/// Find references to the symbol at the given point throughout the project.
pub struct References;

impl<'a> Action<'a> for References {
    type Params = ReferenceParams;
    const METHOD: &'static str = "textDocument/references";

    fn new(_: &'a mut LsState) -> Self {
        References
    }
}

impl<'a> RequestAction<'a> for References {
    type Response = Vec<Location>;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "find_all_refs")?;
        let span = ctx.convert_pos_to_span(file_path, params.position);
        let analysis = ctx.analysis.clone();

        let receiver = receive_from_thread(move || {
            analysis.find_all_refs(&span, params.context.include_declaration)
        });

        let result = match receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT)) {
            Ok(Ok(t)) => t,
            _ => vec![],
        };

        Ok(result.iter().map(|item| ls_util::rls_to_location(item)).collect())
    }
}

/// Get a list of possible completions at the given location.
pub struct Completion;

impl<'a> Action<'a> for Completion {
    type Params = TextDocumentPositionParams;
    const METHOD: &'static str = "textDocument/completion";

    fn new(_: &'a mut LsState) -> Self {
        Completion
    }
}

impl<'a> RequestAction<'a> for Completion {
    type Response = Vec<CompletionItem>;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let vfs = ctx.vfs.clone();
        let file_path = parse_file_path!(&params.text_document.uri, "complete")?;

        let receiver = receive_from_thread(move || {
            let cache = racer::FileCache::new(vfs);
            let session = racer::Session::new(&cache);

            let location = pos_to_racer_location(params.position);
            let results = racer::complete_from_file(file_path, location, &session);

            results.map(|comp| completion_item_from_racer_match(comp)).collect()
        });

        let result = receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT))
            .unwrap_or_else(|_| vec![]);

        Ok(result)
    }
}

/// Find all references to the thing at the given location within this document,
/// so they can be highlighted in the editor. In practice, this is very similar
/// to `References`.
pub struct DocumentHighlight;

impl<'a> Action<'a> for DocumentHighlight {
    type Params = TextDocumentPositionParams;
    const METHOD: &'static str = "textDocument/documentHighlight";

    fn new(_: &'a mut LsState) -> Self {
        DocumentHighlight
    }
}

impl<'a> RequestAction<'a> for DocumentHighlight {
    type Response = Vec<lsp_data::DocumentHighlight>;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "highlight")?;
        let span = ctx.convert_pos_to_span(file_path, params.position);
        let analysis = ctx.analysis.clone();

        let receiver = receive_from_thread(move || {
            analysis.find_all_refs(&span, true)
        });

        let result = match receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT)) {
            Ok(Ok(t)) => t,
            _ => vec![],
        };

        let refs: Vec<_> = result.iter().map(|span| lsp_data::DocumentHighlight {
            range: ls_util::rls_to_range(span.range),
            kind: Some(DocumentHighlightKind::Text),
        }).collect();

        Ok(refs)
    }
}

/// Rename the given symbol within the whole project.
pub struct Rename;

impl<'a> Action<'a> for Rename {
    type Params = RenameParams;
    const METHOD: &'static str = "textDocument/rename";

    fn new(_: &'a mut LsState) -> Self {
        Rename
    }
}

impl<'a> RequestAction<'a> for Rename {
    type Response = WorkspaceEdit;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "rename")?;
        let span = ctx.convert_pos_to_span(file_path, params.position);

        let analysis = ctx.analysis.clone();
        let receiver = receive_from_thread(move || {
            macro_rules! unwrap_or_empty {
                ($e: expr) => {
                    match $e {
                        Ok(e) => e,
                        Err(_) => {
                            return vec![];
                        }
                    }
                }
            }

            let id = unwrap_or_empty!(analysis.crate_local_id(&span));
            let def = unwrap_or_empty!(analysis.get_def(id));
            if def.name == "self" || def.name == "Self" {
                return vec![];
            }

            analysis.find_all_refs(&span, true).unwrap_or_else(|_| vec![])
        });

        let result = receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT))
            .unwrap_or_else(|_| vec![]);

        let mut edits: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        for item in result.iter() {
            let loc = ls_util::rls_to_location(item);
            edits.entry(loc.uri).or_insert_with(Vec::new).push(TextEdit {
                range: loc.range,
                new_text: params.new_name.clone(),
            });
        }

        Ok(WorkspaceEdit { changes: edits })
    }
}

/// Turn wildcard style glob imports (`use foo::*`) into an import of each item
/// that is actually used (`use foo::{Bar, Quux}`).
pub struct Deglob;

impl<'a> Action<'a> for Deglob {
    type Params = Location;
    const METHOD: &'static str = "rustWorkspace/deglob";

    fn new(_: &'a mut LsState) -> Self {
        Deglob
    }
}

impl<'a> RequestAction<'a> for Deglob {
    type Response = Ack;
    fn handle<O: Output>(&mut self, id: usize, location: Self::Params, ctx: &mut ActionContext, out: O) -> Result<Self::Response, ()> {
        let ctx = ctx.inited();
        let span = ls_util::location_to_rls(location.clone());
        let mut span = ignore_non_file_uri!(span, &location.uri, "deglob")?;

        trace!("deglob {:?}", span);

        // Start by checking that the user has selected a glob import.
        if span.range.start() == span.range.end() {
            // search for a glob in the line
            let vfs = ctx.vfs.clone();
            let line = match vfs.load_line(&span.file, span.range.row_start) {
                Ok(l) => l,
                Err(_) => {
                    out.failure_message(id, ErrorCode::InvalidParams, "Could not retrieve line from VFS.");
                    return Err(());
                }
            };

            // search for exactly one "::*;" in the line. This should work fine for formatted text, but
            // multiple use statements could be in the same line, then it is not possible to find which
            // one to deglob.
            let matches: Vec<_> = line.char_indices().filter(|&(_, chr)| chr == '*').collect();
            if matches.len() == 0 {
                out.failure_message(id, ErrorCode::InvalidParams, "No glob in selection.");
                return Err(());
            } else if matches.len() > 1 {
                out.failure_message(id, ErrorCode::InvalidParams, "Multiple globs in selection.");
                return Err(());
            }
            let index = matches[0].0 as u32;
            span.range.col_start = span::Column::new_zero_indexed(index);
            span.range.col_end = span::Column::new_zero_indexed(index+1);
        }

        // Save-analysis exports the deglobbed version of a glob import as its type string.
        let vfs = ctx.vfs.clone();
        let analysis = ctx.analysis.clone();
        let out_clone = out.clone();
        let span_ = span.clone();

        let receiver = receive_from_thread(move || {
            match vfs.load_span(span_.clone()) {
                Ok(ref s) if s != "*" => {
                    out_clone.failure_message(id, ErrorCode::InvalidParams, "Not a glob");
                    return Err("Not a glob");
                }
                Err(e) => {
                    debug!("Deglob failed: {:?}", e);
                    out_clone.failure_message(id, ErrorCode::InternalError, "Couldn't open file");
                    return Err("Couldn't open file");
                }
                _ => {}
            }

            let ty = analysis.show_type(&span_);
            ty.map_err(|_| {
                out_clone.failure_message(id, ErrorCode::InternalError, "Couldn't get info from analysis");
                "Couldn't get info from analysis"
            })
        });

        let result = receiver.recv_timeout(Duration::from_millis(::COMPILER_TIMEOUT));
        let mut deglob_str = match result {
            Ok(Ok(s)) => s,
            _ => {
                return Err(());
            }
        };

        // Handle multiple imports.
        if deglob_str.contains(',') {
            deglob_str = format!("{{{}}}", deglob_str);
        }

        // Send a workspace edit to make the actual change.
        // FIXME should handle the response
        let output = serde_json::to_string(
            &RequestMessage::new(out.provide_id(),
                                 "workspace/applyEdit".to_owned(),
                                 ApplyWorkspaceEditParams { edit: make_workspace_edit(ls_util::rls_to_location(&span), deglob_str) })
        ).unwrap();
        out.response(output);

        // Nothing to actually send in the response.
        Ok(Ack)
    }
}

/// Execute a command within the workspace.
///
/// These are *not* shell commands, but commands given by the client and
/// performed by the RLS.
///
/// Currently, only the "rls.applySuggestion" command is supported.
pub struct ExecuteCommand;

impl<'a> Action<'a> for ExecuteCommand {
    type Params = ExecuteCommandParams;
    const METHOD: &'static str = "workspace/executeCommand";

    fn new(_: &'a mut LsState) -> Self {
        ExecuteCommand
    }
}

impl<'a> RequestAction<'a> for ExecuteCommand {
    type Response = Ack;
    fn handle<O: Output>(&mut self, id: usize, params: Self::Params, _ctx: &mut ActionContext, out: O) -> Result<Self::Response, ()> {
        match &*params.command {
            "rls.applySuggestion" => {
                let location = serde_json::from_value(params.arguments[0].clone()).expect("Bad argument");
                let new_text = serde_json::from_value(params.arguments[1].clone()).expect("Bad argument");
                self.apply_suggestion(id, location, new_text, out)
            }
            c => {
                debug!("Unknown command: {}", c);
                out.failure_message(id, ErrorCode::MethodNotFound, "Unknown command");
                Err(())
            }
        }
    }
}

impl ExecuteCommand {
    fn apply_suggestion<O: Output>(&self, _id: usize, location: Location, new_text: String, out: O) -> Result<Ack, ()> {
        trace!("apply_suggestion {:?} {}", location, new_text);
        // FIXME should handle the response
        let output = serde_json::to_string(
            &RequestMessage::new(out.provide_id(),
                                 "workspace/applyEdit".to_owned(),
                                 ApplyWorkspaceEditParams { edit: make_workspace_edit(location, new_text) })
        ).unwrap();
        out.response(output);
        Ok(Ack)
    }
}

/// Get a list of actions that can be performed on a specific document and range
/// of text by the server.
pub struct CodeAction;

impl<'a> Action<'a> for CodeAction {
    type Params = CodeActionParams;
    const METHOD: &'static str = "textDocument/codeAction";

    fn new(_: &'a mut LsState) -> Self {
        CodeAction
    }
}

impl<'a> RequestAction<'a> for CodeAction {
    type Response = Vec<Command>;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        trace!("code_action {:?}", params);

        let ctx = ctx.inited();
        let file_path = parse_file_path!(&params.text_document.uri, "code_action")?;

        match ctx.previous_build_results.lock().unwrap().get(&file_path) {
            Some(ref diagnostics) => {
                let suggestions = diagnostics.iter().filter(|&&(ref d, _)| d.range == params.range).flat_map(|&(_, ref ss)| ss.iter());
                let mut cmds = vec![];
                for s in suggestions {
                    let span = Location {
                        uri: params.text_document.uri.clone(),
                        range: s.range,
                    };
                    let span = serde_json::to_value(&span).unwrap();
                    let new_text = serde_json::to_value(&s.new_text).unwrap();
                    let cmd = Command {
                        title: s.label.clone(),
                        command: "rls.applySuggestion".to_owned(),
                        arguments: Some(vec![span, new_text]),
                    };
                    cmds.push(cmd);
                }

                Ok(cmds)
            }
            None => {
                Ok(vec![])
            }
        }
    }
}

/// Pretty print the given document.
pub struct Formatting;

impl<'a> Action<'a> for Formatting {
    type Params = DocumentFormattingParams;
    const METHOD: &'static str = "textDocument/formatting";

    fn new(_: &'a mut LsState) -> Self {
        Formatting
    }
}

impl<'a> RequestAction<'a> for Formatting {
    type Response = [TextEdit; 1];
    fn handle<O: Output>(&mut self, id: usize, params: Self::Params, ctx: &mut ActionContext, out: O) -> Result<Self::Response, ()> {
        reformat(id, params.text_document, None, &params.options, ctx, out)
    }
}

/// Pretty print the source within the given location range.
pub struct RangeFormatting;

impl<'a> Action<'a> for RangeFormatting {
    type Params = DocumentRangeFormattingParams;
    const METHOD: &'static str = "textDocument/rangeFormatting";

    fn new(_: &'a mut LsState) -> Self {
        RangeFormatting
    }
}

impl<'a> RequestAction<'a> for RangeFormatting {
    type Response = [TextEdit; 1];
    fn handle<O: Output>(&mut self, id: usize, params: Self::Params, ctx: &mut ActionContext, out: O) -> Result<Self::Response, ()> {
        reformat(id, params.text_document, Some(params.range), &params.options, ctx, out)
    }
}

fn reformat<O: Output>(id: usize, doc: TextDocumentIdentifier, selection: Option<Range>, opts: &FormattingOptions, ctx: &mut ActionContext, out: O) -> Result<[TextEdit; 1], ()> {
    trace!("Reformat: {} {:?} {:?} {} {}", id, doc, selection, opts.tab_size, opts.insert_spaces);
    let ctx = ctx.inited();
    let path = parse_file_path!(&doc.uri, "reformat")?;

    let input = match ctx.vfs.load_file(&path) {
        Ok(FileContents::Text(s)) => FmtInput::Text(s),
        Ok(_) => {
            debug!("Reformat failed, found binary file");
            out.failure_message(id, ErrorCode::InternalError, "Reformat failed to complete successfully");
            return Err(());
        }
        Err(e) => {
            debug!("Reformat failed: {:?}", e);
            out.failure_message(id, ErrorCode::InternalError, "Reformat failed to complete successfully");
            return Err(());
        }
    };

    let range_whole_file = ls_util::range_from_vfs_file(&ctx.vfs, &path);
    let mut config = ctx.fmt_config.get_rustfmt_config().clone();
    if !config.was_set().hard_tabs() {
        config.set().hard_tabs(!opts.insert_spaces);
    }
    if !config.was_set().tab_spaces() {
        config.set().tab_spaces(opts.tab_size as usize);
    }

    if let Some(r) = selection {
        let range_of_rls = ls_util::range_to_rls(r).one_indexed();
        let range = RustfmtRange::new(range_of_rls.row_start.0 as usize, range_of_rls.row_end.0 as usize);
        let mut ranges = HashMap::new();
        ranges.insert("stdin".to_owned(), vec![range]);
        let file_lines = FileLines::from_ranges(ranges);
        config.set().file_lines(file_lines);
    };

    let mut buf = Vec::<u8>::new();
    match format_input(input, &config, Some(&mut buf)) {
        Ok((summary, ..)) => {
            // format_input returns Ok even if there are any errors, i.e., parsing errors.
            if summary.has_no_errors() {
                // Note that we don't need to update the VFS, the client
                // echos back the change to us.
                let text = String::from_utf8(buf).unwrap();

                // If Rustfmt returns range of text that changed,
                // we will be able to pass only range of changed text to the client.
                Ok([TextEdit {
                    range: range_whole_file,
                    new_text: text,
                }])
            } else {
                debug!("reformat: format_input failed: has errors, summary = {:?}", summary);

                out.failure_message(id, ErrorCode::InternalError, "Reformat failed to complete successfully");
                Err(())
            }
        }
        Err(e) => {
            debug!("Reformat failed: {:?}", e);
            out.failure_message(id, ErrorCode::InternalError, "Reformat failed to complete successfully");
            Err(())
        }
    }
}

/// Resolve additional information about the given completion item
/// suggestion. This allows completion items to be yielded as quickly as
/// possible, with more details (which are presumably more expensive to compute)
/// filled in after the initial completion's presentation.
pub struct ResolveCompletion;

impl<'a> Action<'a> for ResolveCompletion {
    type Params = CompletionItem;
    const METHOD: &'static str = "completionItem/resolve";

    fn new(_: &'a mut LsState) -> Self {
        ResolveCompletion
    }
}

impl<'a> RequestAction<'a> for ResolveCompletion {
    type Response = CompletionItem;
    fn handle<O: Output>(&mut self, _id: usize, params: Self::Params, _ctx: &mut ActionContext, _out: O) -> Result<Self::Response, ()> {
        // currently, we safely ignore this as a pass-through since we fully handle
        // textDocument/completion.  In the future, we may want to use this method as a
        // way to more lazily fill out completion information
        Ok(params)
    }
}


fn racer_coord(line: span::Row<span::OneIndexed>,
               column: span::Column<span::ZeroIndexed>)
               -> racer::Coordinate {
    racer::Coordinate {
        line: line.0 as usize,
        column: column.0 as usize,
    }
}

fn from_racer_coord(coord: racer::Coordinate) -> (span::Row<span::OneIndexed>,span::Column<span::ZeroIndexed>) {
    (span::Row::new_one_indexed(coord.line as u32), span::Column::new_zero_indexed(coord.column as u32))
}

fn pos_to_racer_location(pos: Position) -> racer::Location {
    let pos = ls_util::position_to_rls(pos);
    racer::Location::Coords(racer_coord(pos.row.one_indexed(), pos.col))
}

fn location_from_racer_match(a_match: racer::Match) -> Option<Location> {
    let source_path = &a_match.filepath;

    a_match.coords.map(|coord| {
        let (row, col) = from_racer_coord(coord);
        let loc = span::Location::new(row.zero_indexed(), col, source_path);
        ls_util::rls_location_to_location(&loc)
    })
}

lazy_static! {
    static ref WORK_POOL: rayon::ThreadPool = rayon::ThreadPool::new(
        rayon::Configuration::default()
            .thread_name(|num| format!("request-worker-{}", num))
            .panic_handler(|err| warn!("{:?}", err))
    ).unwrap();
}

/// Runs work in a new thread on the `WORK_POOL` returning a result `Receiver`
pub fn receive_from_thread<T, F>(work_fn: F) -> mpsc::Receiver<T>
    where T: Send + 'static,
          F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    WORK_POOL.spawn(move || {
        // an error here simply means the work took too long and the receiver has been dropped
        let _ = sender.send(work_fn());
    });
    receiver
}
