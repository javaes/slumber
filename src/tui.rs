pub mod context;
pub mod input;
pub mod message;
#[cfg(test)]
pub mod test_util;
mod util;
pub mod view;

use crate::{
    collection::{Collection, CollectionFile, ProfileId, Recipe, RecipeId},
    config::Config,
    db::{CollectionDatabase, Database},
    http::RequestSeed,
    template::{Prompter, Template, TemplateChunk, TemplateContext},
    tui::{
        context::TuiContext,
        input::Action,
        message::{Message, MessageSender, RequestConfig},
        util::{save_file, signals},
        view::{ModalPriority, PreviewPrompter, RequestState, View},
    },
    util::{Replaceable, ResultExt},
};
use anyhow::{anyhow, Context};
use chrono::Utc;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::Future;
use notify::{event::ModifyKind, RecursiveMode, Watcher};
use ratatui::{prelude::CrosstermBackend, Terminal};
use std::{
    io::{self, Stdout},
    ops::Deref,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver},
    time,
};
use tracing::{debug, error, info, trace};

/// Main controller struct for the TUI. The app uses a React-ish architecture
/// for the view, with a wrapping controller (this struct)
#[derive(Debug)]
pub struct Tui {
    terminal: Term,
    /// Persistence database, for storing request state, UI state, etc.
    database: CollectionDatabase,
    /// Receiver for the async message queue, which allows background tasks and
    /// the view to pass data and trigger side effects. Nobody else gets to
    /// touch this
    messages_rx: UnboundedReceiver<Message>,
    /// Transmitter for the async message queue, which can be freely cloned and
    /// passed around
    messages_tx: MessageSender,
    /// Replaceable allows us to enforce that the view is dropped before being
    /// recreated. The view persists its state on drop, so that has to happen
    /// before the new one is created.
    view: Replaceable<View>,
    collection_file: CollectionFile,
    should_run: bool,
}

type Term = Terminal<CrosstermBackend<Stdout>>;

impl Tui {
    /// Rough **maximum** time for each iteration of the main loop
    const TICK_TIME: Duration = Duration::from_millis(250);

    /// Start the TUI. Any errors that occur during startup will be panics,
    /// because they prevent TUI execution.
    pub async fn start(collection_path: Option<PathBuf>) -> anyhow::Result<()> {
        initialize_panic_handler();
        let collection_path = CollectionFile::try_path(None, collection_path)?;

        // ===== Initialize global state =====
        // This stuff only needs to be set up *once per session*

        let config = Config::load()?;
        // Create a message queue for handling async tasks
        let (messages_tx, messages_rx) = mpsc::unbounded_channel();
        let messages_tx = MessageSender::new(messages_tx);
        // Load a database for this particular collection
        let database = Database::load()?.into_collection(&collection_path)?;
        // Initialize global view context
        TuiContext::init(config);

        // ===== Initialize collection & view =====

        // If the collection fails to load, create an empty one just so we can
        // move along. We'll watch the file and hopefully the user can fix it
        let collection_file = CollectionFile::load(collection_path.clone())
            .await
            .reported(&messages_tx)
            .unwrap_or_else(|| CollectionFile::with_path(collection_path));
        let view =
            View::new(&collection_file, database.clone(), messages_tx.clone());

        // The code to revert the terminal takeover is in `Tui::drop`, so we
        // shouldn't take over the terminal until right before creating the
        // `Tui`.
        let terminal = initialize_terminal()?;

        let app = Tui {
            terminal,
            database,
            messages_rx,
            messages_tx,

            collection_file,
            should_run: true,

            view: Replaceable::new(view),
        };

        app.run().await
    }

    /// Run the main TUI update loop. Any error returned from this is fatal. See
    /// the struct definition for a description of the different phases of the
    /// run loop.
    async fn run(mut self) -> anyhow::Result<()> {
        // Spawn background tasks
        self.listen_for_signals();
        tokio::spawn(
            TuiContext::get()
                .input_engine
                .input_loop(self.messages_tx.clone()),
        );
        // Hang onto this because it stops running when dropped
        let _watcher = self.watch_collection()?;

        // This loop is limited by the rate that messages come in, with a
        // minimum rate enforced by a timeout
        while self.should_run {
            // ===== Draw Phase =====
            // Draw *first* so initial UI state is rendered immediately
            self.terminal.draw(|f| self.view.draw(f))?;

            // ===== Message Phase =====
            // Grab one message out of the queue and handle it. This will block
            // while the queue is empty so we don't waste CPU cycles. The
            // timeout here makes sure we don't block forever, so things like
            // time displays during in-flight requests will update.
            let future =
                time::timeout(Self::TICK_TIME, self.messages_rx.recv());
            if let Ok(message) = future.await {
                // Error would indicate a very weird and fatal bug so we wanna
                // know about it
                let message =
                    message.expect("Message channel dropped while running");
                trace!(?message, "Handling message");
                // If an error occurs, store it so we can show the user
                if let Err(error) = self.handle_message(message) {
                    self.view.open_modal(error, ModalPriority::High);
                }
            }

            // ===== Event Phase =====
            // Let the view handle all queued events
            self.view.handle_events();
        }

        Ok(())
    }

    /// Handle an incoming message. Any error here will be displayed as a modal
    fn handle_message(&mut self, message: Message) -> anyhow::Result<()> {
        match message {
            Message::CollectionStartReload => {
                let future = self.collection_file.reload();
                let messages_tx = self.messages_tx();
                self.spawn(async move {
                    let collection = future.await?;
                    messages_tx.send(Message::CollectionEndReload(collection));
                    Ok(())
                });
            }
            Message::CollectionEndReload(collection) => {
                self.reload_collection(collection);
            }
            Message::CollectionEdit => {
                let path = self.collection_file.path();
                open::that_detached(path).context("Error opening {path:?}")?;
            }

            Message::CopyRequestUrl(request_config) => {
                self.copy_request_url(request_config)?;
            }
            Message::CopyRequestBody(request_config) => {
                self.copy_request_body(request_config)?;
            }
            Message::CopyRequestCurl(request_config) => {
                self.copy_request_curl(request_config)?;
            }
            Message::CopyText(text) => self.view.copy_text(text),
            Message::SaveFile { default_path, data } => {
                self.spawn(save_file(self.messages_tx(), default_path, data));
            }

            Message::Error { error } => {
                self.view.open_modal(error, ModalPriority::High)
            }

            // Manage HTTP life cycle
            Message::HttpBeginRequest(request_config) => {
                self.send_request(request_config)?
            }
            Message::HttpBuildError { error } => {
                self.view
                    .set_request_state(RequestState::BuildError { error });
            }
            Message::HttpLoading { request } => {
                self.view.set_request_state(RequestState::loading(request))
            }
            Message::HttpComplete(result) => {
                let state = match result {
                    Ok(exchange) => RequestState::response(exchange),
                    Err(error) => RequestState::RequestError { error },
                };
                self.view.set_request_state(state);
            }

            // Force quit short-circuits the view/message cycle, to make sure
            // it doesn't get ate by text boxes
            Message::Input {
                action: Some(Action::ForceQuit),
                ..
            } => self.quit(),
            Message::Input { event, action } => {
                self.view.handle_input(event, action);
            }

            Message::Notify(message) => self.view.notify(message),
            Message::PromptStart(prompt) => {
                self.view.open_modal(prompt, ModalPriority::Low);
            }
            Message::ConfirmStart(confirm) => {
                self.view.open_modal(confirm, ModalPriority::Low);
            }

            Message::TemplatePreview {
                template,
                profile_id,
                destination,
            } => {
                self.render_template_preview(
                    template,
                    profile_id,
                    destination,
                )?;
            }

            Message::Quit => self.quit(),
        }
        Ok(())
    }

    /// Get a cheap clone of the message queue transmitter
    fn messages_tx(&self) -> MessageSender {
        self.messages_tx.clone()
    }

    /// Spawn a task to listen in the backgrouns for quit signals
    fn listen_for_signals(&self) {
        let messages_tx = self.messages_tx();
        self.spawn(async move {
            signals().await?;
            messages_tx.send(Message::Quit);
            Ok(())
        });
    }

    /// Spawn a watcher to automatically reload the collection when the file
    /// changes. Return the watcher because it stops when dropped.
    fn watch_collection(&self) -> anyhow::Result<impl Watcher> {
        // Spawn a watcher for the collection file
        let messages_tx = self.messages_tx();
        let f = move |result: notify::Result<_>| {
            match result {
                // Only reload if the file *content* changes
                Ok(
                    event @ notify::Event {
                        kind: notify::EventKind::Modify(ModifyKind::Data(_)),
                        ..
                    },
                ) => {
                    info!(?event, "Collection file changed, reloading");
                    messages_tx.send(Message::CollectionStartReload);
                }
                // Do nothing for other event kinds
                Ok(_) => {}
                Err(err) => {
                    error!(error = %err, "Error watching collection file");
                }
            }
        };
        let mut watcher = notify::recommended_watcher(f)?;
        watcher
            .watch(self.collection_file.path(), RecursiveMode::NonRecursive)?;
        info!(
            path = ?self.collection_file.path(), ?watcher,
            "Watching collection file for changes"
        );
        Ok(watcher)
    }

    /// Reload state with a new collection
    fn reload_collection(&mut self, collection: Collection) {
        self.collection_file.collection = collection;

        // Rebuild the whole view, because tons of things can change. Drop the
        // old one *first* to make sure UI state is saved before being restored
        let database = self.database.clone();
        let messages_tx = self.messages_tx();
        let collection_file = &self.collection_file;
        self.view.replace(move |old| {
            drop(old);
            View::new(collection_file, database, messages_tx)
        });
    }

    /// GOODBYE
    fn quit(&mut self) {
        info!("Initiating graceful shutdown");
        self.should_run = false;
    }

    /// Render URL for a request, then copy it to the clipboard
    fn copy_request_url(
        &self,
        request_config: RequestConfig,
    ) -> anyhow::Result<()> {
        let seed = RequestSeed::new(
            self.get_recipe(&request_config.recipe_id)?,
            request_config.options,
        );
        let template_context =
            self.template_context(request_config.profile_id, true)?;
        let messages_tx = self.messages_tx();
        // Spawn a task to do the render+copy
        self.spawn(async move {
            let url = TuiContext::get()
                .http_engine
                .build_url(seed, &template_context)
                .await?;
            messages_tx.send(Message::CopyText(url.to_string()));
            Ok(())
        });
        Ok(())
    }

    /// Render body for a request, then copy it to the clipboard
    fn copy_request_body(
        &self,
        request_config: RequestConfig,
    ) -> anyhow::Result<()> {
        let seed = RequestSeed::new(
            self.get_recipe(&request_config.recipe_id)?,
            request_config.options,
        );
        let template_context =
            self.template_context(request_config.profile_id, true)?;
        let messages_tx = self.messages_tx();
        // Spawn a task to do the render+copy
        self.spawn(async move {
            let body = TuiContext::get()
                .http_engine
                .build_body(seed, &template_context)
                .await?
                .ok_or(anyhow!("Request has no body"))?;
            // Clone the bytes :(
            let body = String::from_utf8(body.into())
                .context("Cannot copy request body")?;
            messages_tx.send(Message::CopyText(body));
            Ok(())
        });
        Ok(())
    }

    /// Render a request, then copy the equivalent curl command to the clipboard
    fn copy_request_curl(
        &self,
        request_config: RequestConfig,
    ) -> anyhow::Result<()> {
        let seed = RequestSeed::new(
            self.get_recipe(&request_config.recipe_id)?,
            request_config.options,
        );
        let template_context =
            self.template_context(request_config.profile_id, true)?;
        let messages_tx = self.messages_tx();
        // Spawn a task to do the render+copy
        self.spawn(async move {
            let ticket = TuiContext::get()
                .http_engine
                .build(seed, &template_context)
                .await?;
            let command = ticket.record().to_curl()?;
            messages_tx.send(Message::CopyText(command));
            Ok(())
        });
        Ok(())
    }

    /// Launch an HTTP request in a separate task
    fn send_request(
        &mut self,
        RequestConfig {
            profile_id,
            recipe_id,
            options,
        }: RequestConfig,
    ) -> anyhow::Result<()> {
        // Launch the request in a separate task so it doesn't block.
        // These clones are all cheap.

        let template_context =
            self.template_context(profile_id.clone(), true)?;
        let messages_tx = self.messages_tx();

        // Mark request state as building
        let initialized =
            RequestSeed::new(self.get_recipe(&recipe_id)?, options);
        self.view.set_request_state(RequestState::Building {
            id: initialized.id,
            start_time: Utc::now(),
            profile_id,
            recipe_id,
        });

        // We can't use self.spawn here because HTTP errors are handled
        // differently from all other error types
        let database = self.database.clone();
        tokio::spawn(async move {
            // Build the request
            let ticket = TuiContext::get()
                .http_engine
                .build(initialized, &template_context)
                .await
                .map_err(|error| {
                    // Report the error, but don't actually return anything
                    messages_tx.send(Message::HttpBuildError { error });
                })?;

            // Report liftoff
            messages_tx.send(Message::HttpLoading {
                request: Arc::clone(ticket.record()),
            });

            // Send the request and report the result to the main thread
            let result = ticket.send(&database).await;
            messages_tx.send(Message::HttpComplete(result));

            // By returning an empty result, we can use `?` to break out early.
            // `return` and `break` don't work in an async block :/
            Ok::<(), ()>(())
        });

        Ok(())
    }

    /// Get a recipe by ID. This will clone the recipe, so use it sparingly.
    /// Return an error if the recipe doesn't exist. Generally if this is called
    /// with an unknown ID that indicates a logic error elsewhere, but it
    /// shouldn't be considered fatal.
    fn get_recipe(&self, recipe_id: &RecipeId) -> anyhow::Result<Recipe> {
        let recipe = self
            .collection_file
            .collection
            .recipes
            .get_recipe(recipe_id)
            .ok_or_else(|| anyhow!("No recipe with ID `{recipe_id}`"))?;
        Ok(recipe.clone())
    }

    /// Spawn a task to render a template, storing the result in a pre-defined
    /// lock. As this is a preview, the user will *not* be prompted for any
    /// input. A placeholder value will be used for any prompts.
    fn render_template_preview(
        &self,
        template: Template,
        profile_id: Option<ProfileId>,
        destination: Arc<OnceLock<Vec<TemplateChunk>>>,
    ) -> anyhow::Result<()> {
        let context = self.template_context(profile_id, false)?;
        self.spawn(async move {
            // Render chunks, then write them to the output destination
            let chunks = template.render_chunks(&context).await;
            // If this fails, it's a logic error somewhere. Only one task should
            // exist per lock
            destination.set(chunks).map_err(|_| {
                anyhow!("Multiple writes to template preview lock")
            })
        });
        Ok(())
    }

    /// Helper for spawning a fallible task. Any error in the resolved future
    /// will be shown to the user in a modal.
    fn spawn(
        &self,
        future: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    ) {
        let messages_tx = self.messages_tx();
        tokio::spawn(async move { future.await.reported(&messages_tx) });
    }

    /// Expose app state to the templater. Most of the data has to be cloned out
    /// to be passed across async boundaries. This is annoying but in reality
    /// it should be small data.
    fn template_context(
        &self,
        profile_id: Option<ProfileId>,
        real_prompt: bool,
    ) -> anyhow::Result<TemplateContext> {
        let context = TuiContext::get();
        let prompter: Box<dyn Prompter> = if real_prompt {
            Box::new(self.messages_tx())
        } else {
            Box::new(PreviewPrompter)
        };
        let collection = &self.collection_file.collection;

        Ok(TemplateContext {
            selected_profile: profile_id,
            collection: collection.clone(),
            http_engine: Some(context.http_engine.clone()),
            database: self.database.clone(),
            overrides: Default::default(),
            prompter,
            recursion_count: Default::default(),
        })
    }
}

/// Restore terminal on app exit
impl Drop for Tui {
    fn drop(&mut self) {
        if let Err(err) = restore_terminal() {
            error!(error = err.deref(), "Error restoring terminal, sorry!");
        }
    }
}

/// Restore terminal state during a panic
fn initialize_panic_handler() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal().unwrap();
        original_hook(panic_info);
    }));
}

/// Set up terminal for TUI
fn initialize_terminal() -> anyhow::Result<Term> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Return terminal to initial state
fn restore_terminal() -> anyhow::Result<()> {
    debug!("Restoring terminal");
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    Ok(())
}
