use crate::api::{self, BoardTask, Mutation, TaskStatus};
use leptos::{html, prelude::*, task::spawn_local, wasm_bindgen::JsCast, web_sys};

#[derive(Clone, Copy)]
struct Board {
    tasks: RwSignal<Vec<BoardTask>>,
    loading: RwSignal<bool>,
    pending: RwSignal<bool>,
    loaded: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    notice: RwSignal<String>,
    query: RwSignal<String>,
    status: RwSignal<Option<TaskStatus>>,
    editing: RwSignal<Option<String>>,
    title: RwSignal<String>,
    description: RwSignal<String>,
    title_error: RwSignal<Option<String>>,
    confirming_delete: RwSignal<Option<String>>,
    title_input: NodeRef<html::Input>,
    list_heading: NodeRef<html::H2>,
}

impl Board {
    fn busy(self) -> bool {
        self.loading.get() || self.pending.get()
    }

    fn busy_untracked(self) -> bool {
        self.loading.get_untracked() || self.pending.get_untracked()
    }

    fn focus_list(self) {
        if let Some(heading) = self.list_heading.get() {
            let _ = heading.focus();
        }
    }

    /// Return keyboard focus to a task's status control once the reloaded row
    /// is rendered and re-enabled; fall back to the list heading when the task
    /// is no longer shown (for example, filtered out by its new status).
    fn focus_status(self, id: &str) {
        let element_id = format!("task-status-{id}");
        let list_heading = self.list_heading;
        request_animation_frame(move || {
            let target = document()
                .get_element_by_id(&element_id)
                .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok());
            if let Some(target) = target {
                let _ = target.focus();
            } else if let Some(heading) = list_heading.get_untracked() {
                let _ = heading.focus();
            }
        });
    }

    fn reset_form(self) {
        self.editing.set(None);
        self.title.set(String::new());
        self.description.set(String::new());
        self.title_error.set(None);
    }

    fn edit(self, task: &BoardTask) {
        if self.busy_untracked() {
            return;
        }
        self.editing.set(Some(task.id.clone()));
        self.title.set(task.title.clone());
        self.description.set(task.description.clone());
        self.confirming_delete.set(None);
        self.notice
            .set("Editing task. Save changes or cancel in the task editor.".to_owned());
        if let Some(input) = self.title_input.get() {
            let _ = input.focus();
        }
    }

    async fn load(self) -> Result<(), String> {
        self.loading.set(true);
        let query = self.query.get_untracked();
        let status = self.status.get_untracked();
        let result = api::list(&query, status).await;
        self.loading.set(false);
        match result {
            Ok(tasks) => {
                self.tasks.set(tasks);
                self.loaded.set(true);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn refresh(self) {
        if self.busy_untracked() {
            return;
        }
        self.error.set(None);
        self.notice.set(String::new());
        self.loading.set(true);
        spawn_local(async move {
            match self.load().await {
                Ok(()) => self.notice.set("Task list loaded from server.".to_owned()),
                Err(error) => self.error.set(Some(error)),
            }
        });
    }

    fn mutate(self, mutation: Mutation) {
        if self.busy_untracked() {
            return;
        }
        self.pending.set(true);
        self.error.set(None);
        self.notice.set(String::new());
        spawn_local(async move {
            let edits_form = mutation.edits_form()
                || matches!(&mutation, Mutation::Delete { id }
                    if self.editing.get_untracked().as_ref() == Some(id));
            let message = mutation.success_message();
            let status_task = match &mutation {
                Mutation::Status { id, .. } => Some(id.clone()),
                _ => None,
            };
            match mutation.execute().await {
                Ok(()) => {
                    if edits_form {
                        self.reset_form();
                    }
                    self.confirming_delete.set(None);
                    self.notice.set(message.to_owned());
                    if let Err(error) = self.load().await {
                        self.error.set(Some(format!(
                            "{message} The list could not be refreshed: {error}"
                        )));
                    }
                    if status_task.is_none() {
                        self.focus_list();
                    }
                }
                Err(error) => self.error.set(Some(error)),
            }
            self.pending.set(false);
            // The status control was disabled (and its row re-rendered) while
            // saving; keep the keyboard user on it instead of the list heading.
            if let Some(id) = status_task {
                self.focus_status(&id);
            }
        });
    }
}

#[component]
pub fn App() -> impl IntoView {
    let board = Board {
        tasks: RwSignal::new(Vec::new()),
        loading: RwSignal::new(false),
        pending: RwSignal::new(false),
        loaded: RwSignal::new(false),
        error: RwSignal::new(None),
        notice: RwSignal::new(String::new()),
        query: RwSignal::new(String::new()),
        status: RwSignal::new(None),
        editing: RwSignal::new(None),
        title: RwSignal::new(String::new()),
        title_error: RwSignal::new(None),
        description: RwSignal::new(String::new()),
        confirming_delete: RwSignal::new(None),
        title_input: NodeRef::new(),
        list_heading: NodeRef::new(),
    };
    let filter_query = RwSignal::new(String::new());
    let filter_status = RwSignal::new(String::new());
    board.refresh();

    view! {
        <a class="skip-link" href="#task-list">"Skip to tasks"</a>
        <main>
            <header class="page-header">
                <div class="workspace-title">
                    <span class="workspace-name">"DACS"</span>
                    <span class="workspace-divider" aria-hidden="true">"/"</span>
                    <h1>"Task board"</h1>
                </div>
                <button type="button" class="secondary" disabled=move || board.busy()
                    on:click=move |_| board.refresh()>"Refresh from server"</button>
            </header>

            <div class="connection" role="status" aria-live="polite" aria-atomic="true">
                {move || {
                    if board.pending.get() {
                        "Saving to server…".to_owned()
                    } else if board.loading.get() {
                        "Loading tasks from server…".to_owned()
                    } else if board.error.get().is_some() {
                        "Last request failed. Check the error below.".to_owned()
                    } else if board.loaded.get() {
                        board.notice.get()
                    } else {
                        "Not connected to the task-board server.".to_owned()
                    }
                }}
            </div>
            <Show when=move || board.error.get().is_some()>
                <div class="error" role="alert">
                    <strong>"Request error"</strong>
                    <p>{move || board.error.get().unwrap_or_default()}</p>
                    <p>{move || if board.loaded.get() {
                        "The last loaded snapshot is shown; it may be out of date or not match newly selected filters. Refresh to reconcile server state."
                    } else {
                        "No task list has been loaded. Check that the private server is running, then refresh."
                    }}</p>
                    <button type="button" class="secondary" disabled=move || board.busy()
                        on:click=move |_| board.refresh()>"Retry loading tasks"</button>
                </div>
            </Show>

            <div class="workspace">
                <section class="editor" aria-labelledby="editor-heading">
                    <h2 id="editor-heading">{move || if board.editing.get().is_some() { "Edit task" } else { "Create task" }}</h2>
                    <form on:submit=move |event| {
                        event.prevent_default();
                        if board.busy_untracked() { return; }
                        let title = board.title.get_untracked();
                        let description = board.description.get_untracked();
                        if title.trim().is_empty() {
                            // A validation problem, not a server failure: report it at the field.
                            board.title_error.set(Some("Enter a title containing at least one non-space character.".to_owned()));
                            if let Some(input) = board.title_input.get() { let _ = input.focus(); }
                            return;
                        }
                        board.title_error.set(None);
                        let mutation = match board.editing.get_untracked() {
                            Some(id) => Mutation::Edit { id, title, description },
                            None => Mutation::Create { title, description },
                        };
                        board.mutate(mutation);
                    }>
                        <fieldset disabled=move || board.busy()>
                            <legend class="visually-hidden">"Task details"</legend>
                            <label for="task-title">"Title "<span class="required">"(required)"</span></label>
                            <input id="task-title" name="title" type="text" required node_ref=board.title_input
                                aria-invalid=move || board.title_error.get().is_some().then_some("true")
                                aria-describedby=move || board.title_error.get().is_some().then_some("task-title-error")
                                prop:value=move || board.title.get()
                                on:input=move |event| {
                                    board.title.set(event_target_value(&event));
                                    board.title_error.set(None);
                                } />
                            <Show when=move || board.title_error.get().is_some()>
                                <p id="task-title-error" class="field-error" role="alert">
                                    {move || board.title_error.get().unwrap_or_default()}
                                </p>
                            </Show>
                            <label for="task-description">"Description"</label>
                            <textarea id="task-description" name="description" rows="4"
                                prop:value=move || board.description.get()
                                on:input=move |event| board.description.set(event_target_value(&event)) />
                            <p class="hint">"New tasks start in Todo. Change their status in the task list."</p>
                            <div class="actions">
                                <button type="submit">{move || if board.editing.get().is_some() { "Save changes" } else { "Create task" }}</button>
                                <Show when=move || board.editing.get().is_some()>
                                    <button type="button" class="secondary" on:click=move |_| {
                                        board.reset_form();
                                        board.notice.set("Editing cancelled; no changes saved.".to_owned());
                                        if let Some(input) = board.title_input.get() { let _ = input.focus(); }
                                    }>"Cancel editing"</button>
                                </Show>
                            </div>
                        </fieldset>
                    </form>
                </section>

                <section class="task-area" aria-labelledby="task-list" aria-busy=move || board.busy().to_string()>
                    <h2 id="task-list" tabindex="-1" node_ref=board.list_heading>"Tasks"</h2>
                    <form class="filters" on:submit=move |event| {
                        event.prevent_default();
                        if board.busy_untracked() { return; }
                        board.query.set(filter_query.get_untracked());
                        board.status.set(TaskStatus::parse(&filter_status.get_untracked()));
                        board.refresh();
                    }>
                        <fieldset class="filter-toolbar" disabled=move || board.busy()>
                            <legend class="visually-hidden">"Filter tasks"</legend>
                            <div class="search-field">
                                <label for="filter-text">"Search title or description"</label>
                                <input id="filter-text" name="q" type="search"
                                    prop:value=move || filter_query.get()
                                    on:input=move |event| filter_query.set(event_target_value(&event)) />
                            </div>
                            <div class="filter-status">
                                <label for="filter-status">"Status filter"</label>
                                <select id="filter-status" name="status" prop:value=move || filter_status.get()
                                    on:change=move |event| filter_status.set(event_target_value(&event))>
                                    <option value="">"All statuses"</option>
                                    <option value="todo">"Todo"</option>
                                    <option value="in_progress">"In progress"</option>
                                    <option value="done">"Done"</option>
                                </select>
                            </div>
                            <div class="actions">
                                <button type="submit" class="secondary">"Apply filters"</button>
                                <button type="button" class="secondary" on:click=move |_| {
                                    filter_query.set(String::new());
                                    filter_status.set(String::new());
                                    board.query.set(String::new());
                                    board.status.set(None);
                                    board.refresh();
                                }>"Clear filters"</button>
                            </div>
                        </fieldset>
                    </form>
                    <Show when=move || board.loaded.get()>
                        <p class="list-summary" role="status">{move || {
                            let count = board.tasks.with(Vec::len);
                            format!("{count} {} in the last server response", if count == 1 { "task" } else { "tasks" })
                        }}</p>
                        <Show when=move || board.tasks.with(Vec::is_empty)>
                            <p class="empty">"No tasks match these filters. Create a task or clear the filters."</p>
                        </Show>
                        <ul class="task-list">
                            <For each=move || board.tasks.get() key=|task| task.clone()
                                children=move |task| view! { <TaskRow task board /> } />
                        </ul>
                    </Show>
                </section>
            </div>
        </main>
    }
}

#[component]
fn TaskRow(task: BoardTask, board: Board) -> impl IntoView {
    let status_select = NodeRef::<html::Select>::new();
    let delete_button = NodeRef::<html::Button>::new();
    // Cancelling removes the focused confirmation; return focus to Delete.
    let keep_task = move || {
        board.confirming_delete.set(None);
        if let Some(button) = delete_button.get_untracked() {
            let _ = button.focus();
        }
    };
    let edit_task = task.clone();
    let status_id = task.id.clone();
    let confirm_id = task.id.clone();
    let expanded_id = task.id.clone();
    let show_confirmation_id = task.id.clone();
    let delete_id = task.id.clone();
    let title = task.title.clone();
    let heading_id = format!("task-heading-{}", task.id);
    let labelled_by = heading_id.clone();
    let select_id = format!("task-status-{}", task.id);
    let status_label = format!("Status for {}", task.title);
    let edit_label = format!("Edit {}", task.title);
    let delete_label = format!("Delete {}", task.title);
    let original_status = task.status;
    view! {
        <li class="task-row">
            <article aria-labelledby=labelled_by>
                <div class="task-content">
                    <h3 id=heading_id>{task.title}</h3>
                    {(!task.description.is_empty()).then(|| view! {
                        <p class="description">{task.description}</p>
                    })}
                </div>
                <div class="task-controls">
                    <div class="status-control">
                        <label for=select_id.clone()>"Status"</label>
                        <select id=select_id aria-label=status_label node_ref=status_select data-status=original_status.wire()
                            disabled=move || board.busy() prop:value=original_status.wire()
                            on:change=move |event| {
                                let selected = TaskStatus::parse(&event_target_value(&event));
                                // Keep displaying the persisted status until the server confirms it.
                                if let Some(select) = status_select.get() { select.set_value(original_status.wire()); }
                                if let Some(status) = selected.filter(|status| *status != original_status) {
                                    board.mutate(Mutation::Status { id: status_id.clone(), status });
                                }
                            }>
                            <option value="todo" selected={original_status == TaskStatus::Todo}>"Todo"</option>
                            <option value="in_progress" selected={original_status == TaskStatus::InProgress}>"In progress"</option>
                            <option value="done" selected={original_status == TaskStatus::Done}>"Done"</option>
                        </select>
                    </div>
                    <div class="actions">
                        <button type="button" class="secondary" aria-label=edit_label disabled=move || board.busy()
                            on:click=move |_| board.edit(&edit_task)>"Edit"</button>
                        <button type="button" class="danger secondary" aria-label=delete_label node_ref=delete_button
                            aria-expanded=move || (board.confirming_delete.get().as_ref() == Some(&expanded_id)).to_string()
                            disabled=move || board.busy()
                            on:click=move |_| board.confirming_delete.set(Some(confirm_id.clone()))>"Delete"</button>
                    </div>
                </div>
                <Show when=move || board.confirming_delete.get().as_ref() == Some(&show_confirmation_id)>
                    <div class="delete-confirmation" role="group" aria-label="Confirm task deletion"
                        on:keydown=move |event| {
                            if event.key() == "Escape" && !board.busy_untracked() {
                                keep_task();
                            }
                        }>
                        <p>{format!("Permanently delete “{title}”? This cannot be undone.")}</p>
                        <div class="actions">
                            <button type="button" class="danger" disabled=move || board.busy()
                                on:click={let id = delete_id.clone(); move |_| {
                                    board.mutate(Mutation::Delete { id: id.clone() });
                                }}>"Confirm delete"</button>
                            <button type="button" class="secondary" disabled=move || board.busy()
                                on:click=move |_| keep_task()>"Keep task"</button>
                        </div>
                    </div>
                </Show>
            </article>
        </li>
    }
}
