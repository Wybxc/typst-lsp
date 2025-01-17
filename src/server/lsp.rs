use anyhow::Context;
use futures::FutureExt;
use serde_json::Value as JsonValue;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc, LanguageServer};
use typst::ide::autocomplete;

use crate::config::{
    get_config_registration, Config, ConstConfig, ExportPdfMode, SemanticTokensMode,
};
use crate::ext::InitializeParamsExt;
use crate::lsp_typst_boundary::{lsp_to_typst, typst_to_lsp};

use super::command::LspCommand;
use super::semantic_tokens::{
    get_semantic_tokens_options, get_semantic_tokens_registration,
    get_semantic_tokens_unregistration,
};
use super::TypstServer;

#[tower_lsp::async_trait]
impl LanguageServer for TypstServer {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        self.const_config
            .set(ConstConfig::from(&params))
            .expect("const config should not yet be initialized");

        if let Some(init) = &params.initialization_options {
            let mut config = self.config.write().await;
            config
                .update(init)
                .await
                .as_ref()
                .map_err(ToString::to_string)
                .map_err(jsonrpc::Error::invalid_params)?;
        }

        self.register_workspace_files(&params).await?;

        let config = self.config.read().await;
        let semantic_tokens_provider = match config.semantic_tokens {
            SemanticTokensMode::Enable
                if !params.supports_semantic_tokens_dynamic_registration() =>
            {
                Some(get_semantic_tokens_options().into())
            }
            _ => None,
        };

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        String::from("#"),
                        String::from("."),
                        String::from("@"),
                    ]),
                    ..Default::default()
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                semantic_tokens_provider,
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: LspCommand::all_as_string(),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let const_config = self.get_const_config();
        let mut config = self.config.write().await;

        if const_config.supports_semantic_tokens_dynamic_registration {
            let client = self.client.clone();
            let register = move || {
                let client = client.clone();
                async move {
                    let options = get_semantic_tokens_options();
                    client
                        .register_capability(vec![get_semantic_tokens_registration(options)])
                        .await
                        .context("could not register semantic tokens")
                }
            };

            let client = self.client.clone();
            let unregister = move || {
                let client = client.clone();
                async move {
                    client
                        .unregister_capability(vec![get_semantic_tokens_unregistration()])
                        .await
                        .context("could not unregister semantic tokens")
                }
            };

            if config.semantic_tokens == SemanticTokensMode::Enable {
                if let Some(err) = register().await.err() {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("could not dynamically register semantic tokens: {err}"),
                        )
                        .await;
                }
            }

            config.listen_semantic_tokens(Box::new(move |mode| match mode {
                SemanticTokensMode::Enable => register().boxed(),
                SemanticTokensMode::Disable => unregister().boxed(),
            }));
        }

        if const_config.supports_config_change_registration {
            let err = self
                .client
                .register_capability(vec![get_config_registration()])
                .await
                .err();
            if let Some(err) = err {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("could not register to watch config changes: {err}"),
                    )
                    .await;
            }
        }

        let watch_files_error = self
            .client
            .register_capability(vec![self.get_watcher_registration()])
            .await
            .err();

        if let Some(error) = watch_files_error {
            self.client
                .log_message(
                    MessageType::ERROR,
                    format!("could not register to watch Typst files: {error}"),
                )
                .await;
        }

        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        let mut workspace = self.workspace.write().await;
        if let Err(error) = workspace.sources.insert_open(&uri, text) {
            self.client.log_message(MessageType::ERROR, error).await;
            return;
        }

        let workspace = workspace.downgrade();
        let config = self.config.read().await;

        let source_id = workspace
            .sources
            .get_id_by_uri(&uri)
            .expect("source should exist just after adding it");

        drop(workspace);

        let world = self.get_world_with_main(source_id).await;
        let source = world
            .get_workspace()
            .sources
            .get_open_source_by_id(source_id);

        // the following is useful to debug AST stuff
        /* let message = {
            let root = LinkedNode::new(source.as_ref().root());
            LogMessage {
                message_type: MessageType::LOG,
                message: format!("{root:#?}"),
            }
        };
        self.log_to_client(message).await; */

        self.on_source_changed(&world, &config, source).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;

        let mut workspace = self.workspace.write().await;
        workspace.sources.close(&uri);

        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let changes = params.content_changes;

        let mut workspace = self.workspace.write().await;
        let source_id = workspace
            .sources
            .get_id_by_uri(&uri)
            .expect("source should exist after being changed");

        let source = workspace.sources.get_mut_open_source_by_id(source_id);
        for change in changes {
            self.apply_single_document_change(source, change);
        }

        drop(workspace);

        let world = self.get_world_with_main(source_id).await;
        let config = self.config.read().await;

        let source = world
            .get_workspace()
            .sources
            .get_open_source_by_id(source_id);

        self.client.semantic_tokens_refresh().await.unwrap();

        self.on_source_changed(&world, &config, source).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;

        let (world, source_id) = self.get_world_with_main_uri(&uri).await;
        let config = self.config.read().await;

        let source = world
            .get_workspace()
            .sources
            .get_open_source_by_id(source_id);

        if config.export_pdf == ExportPdfMode::OnSave {
            self.run_diagnostics_and_export(&world, source).await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let changes = params.changes;

        let mut workspace = self.workspace.write().await;

        for change in changes {
            self.handle_file_change_event(&mut workspace, change);
        }
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> jsonrpc::Result<Option<JsonValue>> {
        let ExecuteCommandParams {
            command,
            arguments,
            work_done_progress_params: _,
        } = params;
        self.client.log_message(MessageType::INFO, &command).await;
        match LspCommand::parse(&command) {
            Some(LspCommand::ExportPdf) => {
                self.command_export_pdf(arguments).await?;
            }
            Some(LspCommand::ClearCache) => {
                self.command_clear_cache(arguments).await?;
            }
            None => {
                return Err(jsonrpc::Error::method_not_found());
            }
        };
        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let (world, source_id) = self.get_world_with_main_uri(uri).await;
        let source = world
            .get_workspace()
            .sources
            .get_open_source_by_id(source_id);

        Ok(self.get_hover(&world, source, position))
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> jsonrpc::Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let explicit = params
            .context
            .map(|context| context.trigger_kind == CompletionTriggerKind::INVOKED)
            .unwrap_or(false);

        let (world, source_id) = self.get_world_with_main_uri(uri).await;

        let source = world
            .get_workspace()
            .sources
            .get_open_source_by_id(source_id);

        let typst_offset = lsp_to_typst::position_to_offset(
            position,
            self.get_const_config().position_encoding,
            source.as_ref(),
        );

        let completions = autocomplete(&world, &[], source.as_ref(), typst_offset, explicit);

        match completions {
            Some((_, c)) => {
                let lsp_completions = c.iter().map(typst_to_lsp::completion).collect();
                return Ok(Some(CompletionResponse::Array(lsp_completions)));
            }
            None => {
                return Ok(None);
            }
        }
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> jsonrpc::Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let (world, source_id) = self.get_world_with_main_uri(uri).await;

        let source = world
            .get_workspace()
            .sources
            .get_open_source_by_id(source_id);

        Ok(self.get_signature_at_position(&world, source, position))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> jsonrpc::Result<Option<DocumentSymbolResponse>> {
        let symbols = self
            .get_document_symbols(&params.text_document.uri, None)
            .await
            .map_err(|e| jsonrpc::Error {
                code: jsonrpc::ErrorCode::InternalError,
                message: format!("Failed to get document symbols: {:#}", e),
                data: None,
            })?;
        Ok(Some(symbols.into()))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> jsonrpc::Result<Option<Vec<SymbolInformation>>> {
        let workspace = self.workspace.read().await;
        let source_manager = &workspace.sources;

        let mut symbols = Vec::new();
        for source_uri in source_manager.get_uris().iter() {
            let mut query = None;
            if !params.query.is_empty() {
                query = Some(params.query.as_str());
            }
            let mut document_symbols =
                self.get_document_symbols(source_uri, query)
                    .await
                    .map_err(|e| jsonrpc::Error {
                        code: jsonrpc::ErrorCode::InternalError,
                        message: format!("Failed to get document symbols: {:#}", e),
                        data: None,
                    })?;
            symbols.append(&mut document_symbols);
        }
        Ok(Some(symbols))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> jsonrpc::Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;

        let workspace = self.workspace.read().await;
        let source = workspace
            .sources
            .get_id_by_uri(uri)
            .map(|id| workspace.sources.get_open_source_by_id(id))
            .ok_or_else(jsonrpc::Error::internal_error)?;

        let (tokens, result_id) = self.get_semantic_tokens_full(source);

        let tokens = SemanticTokens {
            result_id: Some(result_id),
            data: tokens,
        };
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> jsonrpc::Result<Option<SemanticTokensFullDeltaResult>> {
        let uri = &params.text_document.uri;

        let workspace = self.workspace.read().await;
        let source = workspace
            .sources
            .get_id_by_uri(uri)
            .map(|id| workspace.sources.get_open_source_by_id(id))
            .ok_or_else(jsonrpc::Error::internal_error)?;

        let (tokens, result_id) =
            self.try_semantic_tokens_delta_from_result_id(source, &params.previous_result_id);
        match tokens {
            Ok(edits) => Ok(Some(
                SemanticTokensDelta {
                    result_id: Some(result_id),
                    edits,
                }
                .into(),
            )),
            Err(tokens) => Ok(Some(
                SemanticTokens {
                    result_id: Some(result_id),
                    data: tokens,
                }
                .into(),
            )),
        }
    }

    async fn did_change_configuration(&self, _: DidChangeConfigurationParams) {
        // We don't get the actual changed configuration and need to poll for it
        // https://github.com/microsoft/language-server-protocol/issues/676

        let values = self
            .client
            .configuration(Config::get_items())
            .await
            .unwrap();

        let mut config = self.config.write().await;
        match config.update_from_values(values).await {
            Ok(()) => {
                self.client
                    .log_message(MessageType::INFO, "New settings applied")
                    .await;
            }
            Err(err) => {
                self.client.log_message(MessageType::ERROR, err).await;
            }
        }
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> jsonrpc::Result<Option<Vec<SelectionRange>>> {
        let uri = &params.text_document.uri;
        let positions = params.positions;

        let (world, source_id) = self.get_world_with_main_uri(uri).await;
        let source = world
            .get_workspace()
            .sources
            .get_open_source_by_id(source_id);

        Ok(self.get_selection_range(source, &positions))
    }
}
