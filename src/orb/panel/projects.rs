//! Projects in the full window (the sidebar's foot, beside Record and
//! Shots): what the tower knows of the user's work, so New space can draft a
//! space from a ticket. Each project (a `[[setups]]` table, `tower setups
//! --json`) shows its name, its account, what it is, and its repos with
//! what each is for.
//!
//! - **Add** and **Edit** open one plain form: the name, the account (a
//!   pick of `tower accounts`), a line on what it is, and the repos, one per
//!   line, `<folder> — <its role>` (a folder's name is enough). Save runs
//!   `tower setup add "<name>" --account … --about … --repo "<folder>=<role>"
//!   …` (with `--was "<old name>"` when it was renamed, so it keeps its
//!   place); the tower checks each folder and says what it took.
//! - **Remove** asks once in the card, then runs `tower setup remove`.
//! - What a command says shows where it was asked.
//! - `HERDER_TOWER_ORB_PROJECT=new|<name>` (looking at it with test data):
//!   the form opens once, empty or on that project, when the page shows it.

use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px, rgba};
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::{h_flex, v_flex};

use super::home::{LINE, SOFT, acts_row, bay_label, empty_box, mbtn, tg};
use super::launch::{Setup, field, repo_name};
use super::window::whead;
use super::{Panel, glass};

/// The projects' page: the form while one is added or changed, and the one
/// whose removal waits for a second click.
#[derive(Default)]
pub(super) struct ProjectsUi {
    pub form: Option<Form>,
    pub removing: Option<String>,
}

/// The form: a new project (`was` none), or one being changed.
pub(super) struct Form {
    was: Option<String>,
    name: Entity<TextareaState>,
    about: Entity<TextareaState>,
    repos: Entity<TextareaState>,
    account: Option<String>,
}

/// A project's repos as the form's field holds them: a line each, `<folder> — <its role>`.
pub(super) fn repos_text(s: &Setup) -> String {
    s.repos
        .iter()
        .enumerate()
        .map(|(i, r)| match s.roles.get(i).map(|x| x.trim()).filter(|x| !x.is_empty()) {
            Some(role) => format!("{r} — {role}"),
            None => r.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What Save runs: `tower setup add "<name>" --account <a> --about "<line>"
/// --repo "<folder>=<role>" … [--was "<old name>"]`. Why not, as the line
/// the form shows.
pub(super) fn add_args(name: &str, account: Option<&str>, about: &str, repos: &str, was: Option<&str>) -> Result<Vec<String>, String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Give the project a name.".into());
    }
    let lines: Vec<(String, String)> = repos.lines().map(crate::setups::repo_and_role).filter(|(f, _)| !f.is_empty()).collect();
    if lines.is_empty() {
        return Err("Name at least one repo: a line each, “<folder> — <what it is in the project>”.".into());
    }
    let mut args = vec!["setup".to_string(), "add".into(), name.clone()];
    if let Some(a) = account.filter(|a| !a.is_empty()) {
        args.extend(["--account".into(), a.to_string()]);
    }
    let about = about.split_whitespace().collect::<Vec<_>>().join(" ");
    if !about.is_empty() {
        args.extend(["--about".into(), about]);
    }
    for (folder, role) in lines {
        args.extend(["--repo".into(), if role.is_empty() { folder } else { format!("{folder}={role}") }]);
    }
    if let Some(w) = was.filter(|w| !w.trim().eq_ignore_ascii_case(&name)) {
        args.extend(["--was".into(), w.to_string()]);
    }
    Ok(args)
}

impl Panel {
    /// Add (`which` none) or Edit: the form opens with what the project has.
    fn project_form(&mut self, which: Option<Setup>, window: &mut Window, cx: &mut Context<Self>) {
        let make = |text: &str, lines: (usize, usize), hint: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            let f = cx.new(|cx| TextareaState::new(window, cx).auto_grow(lines.0, lines.1).placeholder(hint));
            let text = text.to_string();
            f.update(cx, |s, cx| s.set_value(text, window, cx));
            f
        };
        let s = which.clone().unwrap_or_default();
        let name = make(&s.name, (1, 1), "EYZFlow", window, cx);
        let about = make(&s.about, (1, 2), "What the project is, in a line", window, cx);
        let repos = make(&repos_text(&s), (3, 10), "~/Work/eyz/eyzflow — the web app and its API\n~/Work/eyz/eyz-mediaroom-serverless — the media backend", window, cx);
        name.update(cx, |f, cx| f.focus(window, cx));
        let account = Some(s.account.clone()).filter(|a| !a.is_empty()).or_else(|| self.accounts.as_ref().and_then(|l| l.first().cloned()));
        if let Some(full) = self.full.as_mut() {
            full.notes.remove("project-form");
            full.projects.removing = None;
            full.projects.form = Some(Form { was: which.map(|s| s.name), name, about, repos, account });
        }
        cx.notify();
    }

    /// Save: `tower setup add …`; the list is read again once it is kept.
    fn project_save(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.full.as_ref().and_then(|f| f.projects.form.as_ref()) else { return };
        let (name, about, repos) = (form.name.read(cx).value().to_string(), form.about.read(cx).value().to_string(), form.repos.read(cx).value().to_string());
        let args = match add_args(&name, form.account.as_deref(), &about, &repos, form.was.as_deref()) {
            Ok(a) => a,
            Err(e) => {
                if let Some(full) = self.full.as_mut() {
                    full.notes.insert("project-form".into(), (false, e));
                }
                return cx.notify();
            }
        };
        self.run("project-form", args, cx, |this, result, cx| {
            if result.is_ok() {
                if let Some(full) = this.full.as_mut() {
                    full.projects.form = None;
                }
                this.read_setups(cx);
            }
        });
    }

    /// Remove, once asked again in its card: `tower setup remove "<name>"`.
    fn project_remove(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_mut() else { return };
        if full.projects.removing.as_deref() != Some(name.as_str()) {
            full.projects.removing = Some(name);
            return cx.notify();
        }
        full.projects.removing = None;
        self.run("projects", vec!["setup".into(), "remove".into(), name], cx, |this, result, cx| {
            if result.is_ok() {
                this.read_setups(cx);
            }
        });
    }

    /// The form, in a card at the top.
    fn project_form_view(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let label = |text: &str| bay_label(text).px(px(0.0));
        let hint = |text: &str| div().px(px(4.0)).text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(text.to_string());
        let accounts: AnyElement = match self.accounts.as_deref() {
            None => hint("Reading the accounts…").into_any_element(),
            Some([]) => hint("No accounts mod here: its sessions run on this Mac’s own login.").into_any_element(),
            Some(list) => h_flex()
                .flex_wrap()
                .gap(px(6.0))
                .children(list.iter().enumerate().map(|(i, a)| {
                    let name = a.clone();
                    tg(SharedString::from(format!("project-account-{i}")), a, form.account.as_deref() == Some(a.as_str())).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = this.full.as_mut().and_then(|f| f.projects.form.as_mut()) {
                            f.account = Some(name.clone());
                        }
                        cx.notify();
                    }))
                }))
                .into_any_element(),
        };
        let busy = self.running("project-form");
        v_flex()
            .w_full()
            .p(px(14.0))
            .gap(px(12.0))
            .rounded(px(16.0))
            .bg(rgba(SOFT))
            .border_1()
            .border_color(rgba(LINE))
            .child(div().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).child(match &form.was {
                Some(name) => format!("Change {name}"),
                None => "A new project".to_string(),
            }))
            .child(v_flex().gap(px(6.0)).child(label("Name")).child(field(&form.name)))
            .child(v_flex().gap(px(6.0)).child(label("Account")).child(accounts))
            .child(v_flex().gap(px(6.0)).child(label("About")).child(field(&form.about)))
            .child(
                v_flex()
                    .gap(px(6.0))
                    .child(label("Repos"))
                    .child(field(&form.repos))
                    .child(hint("One per line: <folder> — <its role>. A folder’s name is enough (eyzflow). The roles are what a draft reads to pick the repos a ticket needs.")),
            )
            .child(
                acts_row()
                    .child(mbtn("project-save", if busy { "Saving…" } else { "Save" }, true, None).when(busy, |el| el.opacity(0.5)).when(!busy, |el| el.on_click(cx.listener(|this, _, _, cx| this.project_save(cx)))))
                    .child(mbtn("project-cancel", "Cancel", false, None).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(full) = this.full.as_mut() {
                            full.projects.form = None;
                            full.notes.remove("project-form");
                        }
                        cx.notify();
                    }))),
            )
            .children(self.said("project-form"))
            .into_any_element()
    }

    /// A project as its card shows it: name and account, what it is, its repos with their roles.
    fn project_card(&self, i: usize, s: &Setup, first: bool, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let removing = self.full.as_ref().is_some_and(|f| f.projects.removing.as_deref() == Some(s.name.as_str()));
        let account = if s.account.is_empty() { "the default account".to_string() } else { format!("account {}", s.account) };
        let (edit, remove) = (s.clone(), s.name.clone());
        let pill = |text: String| div().flex_none().px(px(8.0)).rounded_full().border_1().border_color(rgba(LINE)).text_size(px(11.0)).text_color(g.faint).child(text);
        let repos = v_flex().gap(px(6.0)).children(s.repos.iter().enumerate().map(|(k, r)| {
            let role = s.roles.get(k).map(|x| x.trim()).unwrap_or("");
            h_flex()
                .gap(px(9.0))
                .items_start()
                .child(div().flex_none().text_color(g.muted).child("•"))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap(px(6.0))
                                .items_baseline()
                                .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(repo_name(r).to_string()))
                                .child(div().min_w_0().truncate().text_size(px(11.0)).text_color(g.faint).child(r.clone())),
                        )
                        .child(div().text_size(px(12.0)).line_height(px(16.0)).text_color(if role.is_empty() { g.faint } else { g.muted }).child(if role.is_empty() {
                            "No role yet: Edit says what it is in the project.".to_string()
                        } else {
                            role.to_string()
                        })),
                )
        }));
        v_flex()
            .w_full()
            .p(px(14.0))
            .gap(px(9.0))
            .rounded(px(16.0))
            .bg(rgba(SOFT))
            .text_size(px(13.0))
            .line_height(px(18.0))
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child(s.name.clone()))
                    .child(pill(account))
                    // The one `tower launch` takes when nothing else says which.
                    .when(first, |el| el.child(pill("used by default".into())))
                    .child(div().flex_1())
                    .child(tg(SharedString::from(format!("project-edit-{i}")), "Edit", false).on_click(cx.listener(move |this, _, window, cx| this.project_form(Some(edit.clone()), window, cx))))
                    .child(tg(SharedString::from(format!("project-remove-{i}")), "Remove", false).on_click(cx.listener(move |this, _, _, cx| this.project_remove(remove.clone(), cx)))),
            )
            .child(div().text_color(if s.about.is_empty() { g.faint } else { g.muted }).child(if s.about.is_empty() { "What it is isn’t said yet.".to_string() } else { s.about.clone() }))
            .child(repos)
            .when(removing, |el| {
                let name = s.name.clone();
                el.child(
                    h_flex()
                        .gap(px(8.0))
                        .items_center()
                        .flex_wrap()
                        .child(div().text_color(g.muted).child(format!("Remove {}? New space won’t offer it; its repos stay as they are.", s.name)))
                        .child(mbtn(SharedString::from(format!("project-remove-yes-{i}")), "Remove", true, None).on_click(cx.listener(move |this, _, _, cx| this.project_remove(name.clone(), cx))))
                        .child(mbtn(SharedString::from(format!("project-remove-no-{i}")), "Keep", false, None).on_click(cx.listener(|this, _, _, cx| {
                            if let Some(full) = this.full.as_mut() {
                                full.projects.removing = None;
                            }
                            cx.notify();
                        }))),
                )
            })
            .into_any_element()
    }

    /// `HERDER_TOWER_ORB_PROJECT`: the form opens once, when the projects have been read.
    fn project_hook(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let Ok(which) = std::env::var("HERDER_TOWER_ORB_PROJECT") else { return };
        let Some(list) = self.full.as_ref().and_then(|f| f.launch.setups()).map(|s| s.to_vec()) else { return };
        if DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let one = list.into_iter().find(|s| s.name.eq_ignore_ascii_case(which.trim()));
        self.project_form(one, window, cx);
    }

    /// The projects' head and body.
    pub(super) fn projects_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        self.project_hook(window, cx);
        let open = self.full.as_ref().is_some_and(|f| f.projects.form.is_some());
        let add = (!open).then(|| mbtn("project-add", "+ Add project", true, None).on_click(cx.listener(|this, _, window, cx| this.project_form(None, window, cx))).into_any_element());
        let head = whead(
            "Projects",
            vec![div().child("What the tower knows of your work. New space reads each repo’s role to pick the repos a ticket needs.").into_any_element()],
            add,
        );
        let mut body: Vec<AnyElement> = vec![];
        if let Some(form) = self.full.as_ref().and_then(|f| f.projects.form.as_ref()) {
            body.push(self.project_form_view(form, cx));
        }
        let list = self.full.as_ref().and_then(|f| f.launch.setups()).map(|s| s.to_vec());
        match list.as_deref() {
            None => body.push(empty_box("Asking the tower…")),
            Some([]) if !open => body.push(empty_box("No projects yet. Add one: its repos, and a line on what each is for. Until then a ticket's draft picks among the folders under ~/Work.")),
            Some([]) => {}
            Some(list) => {
                // The one being changed is the form for now.
                let editing = self.full.as_ref().and_then(|f| f.projects.form.as_ref()).and_then(|f| f.was.clone());
                let n = list.len();
                body.extend(list.iter().enumerate().filter(|(_, s)| editing.as_deref() != Some(s.name.as_str())).map(|(i, s)| self.project_card(i, s, i == 0 && n > 1, cx)));
            }
        }
        body.extend(self.said("projects"));
        body.extend(self.said("setups"));
        (head, vec![v_flex().w_full().max_w(px(760.0)).gap(px(12.0)).children(body).into_any_element()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Setup {
        Setup {
            name: "EYZFlow".into(),
            account: "orca".into(),
            about: "the video platform".into(),
            repos: vec!["~/Work/eyz/eyzflow".into(), "~/Work/eyz/eyz-mediaroom-serverless".into()],
            roles: vec!["the web app".into(), String::new()],
        }
    }

    #[test]
    fn the_form_holds_a_repo_a_line_with_its_role() {
        assert_eq!(repos_text(&project()), "~/Work/eyz/eyzflow — the web app\n~/Work/eyz/eyz-mediaroom-serverless");
        assert_eq!(repos_text(&Setup::default()), "");
    }

    #[test]
    fn save_says_the_project_as_tower_setup_add_takes_it() {
        let text = "~/Work/eyz/eyzflow — the web app\n\n  eyz-mediaroom-serverless = the media backend \n~/Work/eyz/qa";
        assert_eq!(
            add_args(" EYZFlow ", Some("orca"), "  the video   platform ", text, None).unwrap(),
            [
                "setup", "add", "EYZFlow", "--account", "orca", "--about", "the video platform", "--repo", "~/Work/eyz/eyzflow=the web app", "--repo", "eyz-mediaroom-serverless=the media backend", "--repo",
                "~/Work/eyz/qa",
            ]
        );
        // Renamed: it keeps its place; the same name in another case is no rename.
        let renamed = add_args("EYZ", None, "", "eyzflow", Some("EYZFlow")).unwrap();
        assert_eq!(renamed, ["setup", "add", "EYZ", "--repo", "eyzflow", "--was", "EYZFlow"]);
        assert!(!add_args("eyzflow", None, "", "eyzflow", Some("EYZFlow")).unwrap().contains(&"--was".to_string()));
        // Nothing to save: said in the form.
        assert!(add_args("  ", None, "", "eyzflow", None).unwrap_err().contains("name"));
        assert!(add_args("x", None, "", " \n ", None).unwrap_err().contains("at least one repo"));
    }
}
