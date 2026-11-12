//! Built-in Git (like Files): the status bar shows the branch and ahead/behind; the sync
//! button offers Pull / Push / Sync / Fetch, the branch button switches or creates branches.
//! Network operations run in the Status Center with git's progress lines.

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName as L;
use gpui_kit::*;
use rf_win::git::{self, CheckoutMode};

use crate::fluent_menu::{self, Item, Menu, MenuIcon};
use crate::folder_view::FolderView;
use crate::prompt::{self, Button, Kind};
use crate::t;

type View = WeakEntity<FolderView>;

fn refresh(view: &View, cx: &mut App) {
    let _ = view.update(cx, |v, cx| v.refresh(cx));
}

/// Pull / Push / Sync / Fetch menu.
pub fn sync_menu(view: View, root: PathBuf, ahead: u32, behind: u32, pos: Point<Pixels>, window: &mut Window, cx: &mut App) {
    type Op = fn(&Path, &mut dyn FnMut(&str)) -> anyhow::Result<()>;
    let item = |label: &str, icon: L, hint: Option<String>, title: &'static str, op: Op| {
        let (view, root) = (view.clone(), root.clone());
        let mut it = Item::action(label.to_string(), MenuIcon::Lucide(icon), move |_, cx| run_op(cx, format!("{}: {}", crate::i18n::t(title), repo_name(&root)), root.clone(), op, view.clone()));
        if let Some(h) = hint {
            it = it.hint(h);
        }
        it
    };
    let remote = git::remote_url(&root);
    let mut items = Vec::new();
    if remote.is_some() {
        items.extend([
            item(t!("Çek (pull)"), L::ArrowDown, (behind > 0).then(|| format!("↓{behind}")), "Çekiliyor", git::pull),
            item(t!("Gönder (push)"), L::ArrowUp, (ahead > 0).then(|| format!("↑{ahead}")), "Gönderiliyor", git::push),
            item(t!("Eşitle"), L::RefreshCw, None, "Eşitleniyor", git::sync),
            Item::Separator,
            item(t!("Uzak depodan getir (fetch)"), L::CloudDownload, None, "Getiriliyor", git::fetch),
            Item::Separator,
        ]);
    }
    {
        let (view, root) = (view.clone(), root.clone());
        items.push(Item::action(t!("Değişiklikleri kaydet (commit)…"), MenuIcon::Lucide(L::GitCommitHorizontal), move |window, cx| commit_dialog(view.clone(), root.clone(), window, cx)));
    }
    {
        let (view, root, current) = (view.clone(), root.clone(), remote.clone());
        let label = if remote.is_some() { t!("Uzak depo adresini değiştir…") } else { t!("Uzak depoya bağla…") };
        items.push(Item::action(label, MenuIcon::Lucide(L::Link), move |window, cx| connect_dialog(view.clone(), root.clone(), current.clone(), window, cx)));
    }
    fluent_menu::show(Menu { quick: Vec::new(), items }, pos, window, cx);
}

fn repo_name(root: &Path) -> String {
    rf_core::history::display_name(root)
}

fn run_op(cx: &mut App, title: String, root: PathBuf, op: fn(&Path, &mut dyn FnMut(&str)) -> anyhow::Result<()>, view: View) {
    crate::status_center::start_with(
        cx,
        title,
        move |t| {
            op(&root, &mut |line: &str| (t.on_progress)(0, 0, Some(line.to_string())))?;
            Ok(true)
        },
        Some(Box::new(move |_, cx| refresh(&view, cx))),
    );
}

/// "Commit changes…": a message, then everything is staged and committed.
fn commit_dialog(view: View, root: PathBuf, window: &mut Window, cx: &mut App) {
    let go = Button::new(t!("Kaydet"), Kind::Primary, move |msg, _, cx| {
        let msg = msg.unwrap_or_default().trim().to_string();
        if msg.is_empty() {
            return Err(t!("Ne değiştiğini kısaca yazın").into());
        }
        let (root, view) = (root.clone(), view.clone());
        let title = format!("{} {}", t!("Kaydediliyor:"), repo_name(&root));
        crate::status_center::start_with(
            cx,
            title,
            move |_| {
                git::commit_all(&root, &msg)?;
                Ok(true)
            },
            Some(Box::new(move |_, cx| refresh(&view, cx))),
        );
        Ok(())
    });
    prompt::show(
        t!("Değişiklikleri kaydet"),
        Some(t!("Bu depodaki bütün değişiklikler (yeni dosyalar dahil) tek bir kayıt olarak saklanır.").to_string()),
        Some((String::new(), t!("Örneğin: Kapak resmi güncellendi").to_string())),
        vec![go, Button::cancel()],
        window,
        cx,
    );
}

/// "Connect to a remote…" / "Change remote address…": sets `origin`; "Connect and push" also
/// sends the current branch (with upstream) right away.
fn connect_dialog(view: View, root: PathBuf, current: Option<String>, window: &mut Window, cx: &mut App) {
    let start = |push: bool, view: View, root: PathBuf| {
        move |url: Option<String>, _: &mut Window, cx: &mut App| -> Result<(), String> {
            let url = url.unwrap_or_default().trim().to_string();
            if url.is_empty() {
                return Err(t!("Depo adresini girin").into());
            }
            let (root, view) = (root.clone(), view.clone());
            let title = format!("{} {}", t!("Uzak depoya bağlanıyor:"), repo_name(&root));
            crate::status_center::start_with(
                cx,
                title,
                move |t| {
                    git::connect_remote(&root, &url)?;
                    if push {
                        git::push(&root, &mut |line: &str| (t.on_progress)(0, 0, Some(line.to_string())))?;
                    }
                    Ok(true)
                },
                Some(Box::new(move |_, cx| refresh(&view, cx))),
            );
            Ok(())
        }
    };
    let mut buttons = Vec::new();
    if current.is_none() {
        buttons.push(Button::new(t!("Bağla ve gönder"), Kind::Primary, start(true, view.clone(), root.clone())));
        buttons.push(Button::new(t!("Yalnızca bağla"), Kind::Normal, start(false, view.clone(), root.clone())));
    } else {
        buttons.push(Button::new(t!("Değiştir"), Kind::Primary, start(false, view.clone(), root.clone())));
    }
    buttons.push(Button::cancel());
    prompt::show(
        if current.is_none() { t!("Uzak depoya bağla") } else { t!("Uzak depo adresi") },
        Some(t!("GitHub, GitLab ya da başka bir sunucudaki boş bir deponun adresi. Önce orada bir depo oluşturun.").to_string()),
        Some((current.unwrap_or_default(), "https://github.com/kullanici/depo.git".to_string())),
        buttons,
        window,
        cx,
    );
}

/// "Initialize Git repository" for a folder that is not in one.
pub fn init_repo(view: View, dir: PathBuf, cx: &mut App) {
    let title = format!("{} {}", t!("Git deposu başlatılıyor:"), repo_name(&dir));
    crate::status_center::start_with(
        cx,
        title,
        move |_| {
            git::init(&dir)?;
            Ok(true)
        },
        Some(Box::new(move |_, cx| refresh(&view, cx))),
    );
}

/// "Clone repository…": asks for the address, clones into a new folder inside `dir`.
pub fn clone_dialog(view: View, dir: PathBuf, window: &mut Window, cx: &mut App) {
    let go = Button::new(t!("Klonla"), Kind::Primary, move |url, _, cx| {
        let url = url.unwrap_or_default().trim().to_string();
        if url.is_empty() {
            return Err(t!("Depo adresini girin").into());
        }
        let (dir, view) = (dir.clone(), view.clone());
        let title = format!("{} {}", t!("Klonlanıyor:"), url.rsplit('/').next().unwrap_or(&url).trim_end_matches(".git"));
        crate::status_center::start_with(
            cx,
            title,
            move |t| {
                git::clone(&url, &dir, &mut |line: &str| (t.on_progress)(0, 0, Some(line.to_string())))?;
                Ok(true)
            },
            Some(Box::new(move |_, cx| {
                if let Some(v) = view.upgrade() {
                    v.update(cx, |v, cx| v.refresh(cx));
                }
            })),
        );
        Ok(())
    });
    prompt::show(
        t!("Depoyu klonla"),
        Some(t!("Depo bu klasörün içinde yeni bir klasöre klonlanır.").to_string()),
        Some((String::new(), "https://github.com/kullanici/depo.git".to_string())),
        vec![go, Button::cancel()],
        window,
        cx,
    );
}

/// Branch list (local, then remote) with "New branch…"; picking one switches to it.
pub fn branch_menu(view: View, root: PathBuf, pos: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let handle = window.window_handle();
    let task = cx.background_executor().spawn({
        let root = root.clone();
        async move { git::branches(&root) }
    });
    cx.spawn(async move |cx| {
        let branches = match task.await {
            Ok(b) => b,
            Err(e) => {
                log::warn!("git branches: {e:#}");
                return;
            }
        };
        let _ = cx.update_window(handle, |_, window, cx| {
            let mut items = Vec::new();
            {
                let (view, root) = (view.clone(), root.clone());
                items.push(Item::action(t!("Yeni dal oluştur…"), MenuIcon::Lucide(L::GitBranchPlus), move |window, cx| {
                    new_branch_dialog(view.clone(), root.clone(), window, cx)
                }));
            }
            items.push(Item::Separator);
            let mut remote = Vec::new();
            for b in branches {
                let hint = match (b.ahead, b.behind) {
                    (0, 0) => None,
                    (a, 0) => Some(format!("↑{a}")),
                    (0, d) => Some(format!("↓{d}")),
                    (a, d) => Some(format!("↑{a} ↓{d}")),
                };
                let (view, root, name) = (view.clone(), root.clone(), b.name.clone());
                let is_head = b.is_head;
                let mut it = Item::action(b.name.clone(), MenuIcon::Lucide(L::GitBranch), move |window, cx| {
                    if !is_head {
                        checkout(view.clone(), root.clone(), name.clone(), CheckoutMode::Normal, window, cx)
                    }
                })
                .checked(b.is_head);
                if let Some(h) = hint {
                    it = it.hint(h);
                }
                if b.is_remote {
                    remote.push(it);
                } else {
                    items.push(it);
                }
            }
            if !remote.is_empty() {
                items.push(Item::Separator);
                items.push(Item::sub(t!("Uzak dallar"), MenuIcon::Lucide(L::Cloud), remote));
            }
            fluent_menu::show(Menu { quick: Vec::new(), items }, pos, window, cx);
        });
    })
    .detach();
}

fn checkout(view: View, root: PathBuf, branch: String, mode: CheckoutMode, window: &mut Window, cx: &mut App) {
    let handle = window.window_handle();
    let task = cx.background_executor().spawn({
        let (root, branch) = (root.clone(), branch.clone());
        async move { git::checkout(&root, &branch, mode).map_err(|e| format!("{e:#}")) }
    });
    cx.spawn(async move |cx| {
        let r = task.await;
        let _ = cx.update_window(handle, |_, window, cx| match r {
            Ok(()) => refresh(&view, cx),
            Err(e) if e.starts_with("Dal değiştirilemedi: yerel değişiklikler") && matches!(mode, CheckoutMode::Normal) => {
                // Files asks what to do with uncommitted changes
                let (v1, r1, b1) = (view.clone(), root.clone(), branch.clone());
                let (v2, r2, b2) = (view.clone(), root.clone(), branch.clone());
                prompt::show(
                    t!("Kaydedilmemiş değişiklikler var"),
                    Some(format!("{} \"{branch}\"", t!("Değişikliklerinizle ne yapılsın? Geçilecek dal:"))),
                    None,
                    vec![
                        Button::new(t!("Yanımda götür"), Kind::Primary, move |_, window, cx| {
                            checkout(v1.clone(), r1.clone(), b1.clone(), CheckoutMode::Stash, window, cx);
                            Ok(())
                        }),
                        Button::new(t!("Değişiklikleri at"), Kind::Danger, move |_, window, cx| {
                            checkout(v2.clone(), r2.clone(), b2.clone(), CheckoutMode::Discard, window, cx);
                            Ok(())
                        }),
                        Button::cancel(),
                    ],
                    window,
                    cx,
                );
            }
            Err(e) => error_dialog(t!("Dal değiştirilemedi"), e, window, cx),
        });
    })
    .detach();
}

fn error_dialog(title: &str, message: String, window: &mut Window, cx: &mut App) {
    prompt::show(title.to_string(), Some(crate::i18n::td(&message)), None, vec![Button::new(t!("Tamam"), Kind::Primary, |_, _, _| Ok(()))], window, cx);
}

fn new_branch_dialog(view: View, root: PathBuf, window: &mut Window, cx: &mut App) {
    prompt::show(
        t!("Yeni dal"),
        Some(t!("Yeni dal geçerli daldan oluşturulur ve ona geçilir.").to_string()),
        Some((String::new(), t!("Dal adı").to_string())),
        vec![
            Button::new(t!("Oluştur"), Kind::Primary, move |value, window, cx| {
                let name = value.unwrap_or_default().trim().to_string();
                if !git::is_valid_branch_name(&name) {
                    return Err(t!("Geçersiz dal adı (boşluk, .., ~ ^ : ? * [ \\ kullanılamaz)").to_string());
                }
                let (view, root) = (view.clone(), root.clone());
                let handle = window.window_handle();
                let task = cx.background_executor().spawn({
                    let (root, name) = (root.clone(), name.clone());
                    async move { git::create_branch(&root, &name, None, true).map_err(|e| format!("{e:#}")) }
                });
                cx.spawn(async move |cx| {
                    let r = task.await;
                    let _ = cx.update_window(handle, |_, window, cx| match r {
                        Ok(()) => refresh(&view, cx),
                        Err(e) => error_dialog(t!("Dal oluşturulamadı"), e, window, cx),
                    });
                })
                .detach();
                Ok(())
            }),
            Button::cancel(),
        ],
        window,
        cx,
    );
}
