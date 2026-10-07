use super::*;

impl Sidebar {
    /// Header "list management" menu: choose one of two views (flat list / grouped
    /// by workspace), with the check mark on the right (aligned with ZCode)
    pub(crate) fn view_menu(
        view: &WeakEntity<Self>,
        current: SidebarView,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + Clone + 'static
    {
        let view = view.clone();
        move |menu, _, _| {
            let item = |label: std::borrow::Cow<'static, str>,
                        icon: AssetsIconName,
                        target: SidebarView| {
                let view = view.clone();
                PopupMenuItem::new(label)
                    .icon(icon)
                    .checked(current == target)
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.view = target;
                            cx.notify();
                        });
                    })
            };
            menu.label(rust_i18n::t!("sidebar.view"))
                .item(item(
                    rust_i18n::t!("sidebar.view_flat"),
                    AssetsIconName::List,
                    SidebarView::Flat,
                ))
                .item(item(
                    rust_i18n::t!("sidebar.view_grouped"),
                    AssetsIconName::FolderKanban,
                    SidebarView::Workspace,
                ))
                .check_side(Side::Right)
        }
    }

    /// Session row context menu: rename / pin / archive / delete (clears the
    /// database plus the rollout, unrecoverable)
    pub(crate) fn session_menu(
        view: &WeakEntity<Self>,
        session: &SidebarSession,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + Clone + 'static
    {
        let view = view.clone();
        let id = session.id.clone();
        let title = session.title.clone();
        let pinned = session.pinned;
        let archived = session.archived;
        move |menu, _, _| {
            let rename_view = view.clone();
            let rename_id = id.clone();
            let rename_title = title.clone();
            let pin_view = view.clone();
            let pin_id = id.clone();
            let archive_view = view.clone();
            let archive_id = id.clone();
            let delete_view = view.clone();
            let delete_id = id.clone();
            menu.item(
                PopupMenuItem::new(rust_i18n::t!("sidebar.rename")).on_click(
                    move |_, window, cx| {
                        let _ = rename_view.update(cx, |this, cx| {
                            this.start_session_rename(
                                rename_id.clone(),
                                rename_title.clone(),
                                window,
                                cx,
                            );
                        });
                    },
                ),
            )
            .item(
                PopupMenuItem::new(if pinned {
                    rust_i18n::t!("sidebar.unpin")
                } else {
                    rust_i18n::t!("sidebar.pin")
                })
                .on_click(move |_, _, cx| {
                    let _ = pin_view.update(cx, |_, cx| {
                        cx.emit(SidebarEvent::SetPinned(pin_id.clone(), !pinned));
                    });
                }),
            )
            .item(
                PopupMenuItem::new(if archived {
                    rust_i18n::t!("sidebar.unarchive")
                } else {
                    rust_i18n::t!("sidebar.archive")
                })
                .on_click(move |_, _, cx| {
                    let _ = archive_view.update(cx, |_, cx| {
                        cx.emit(SidebarEvent::SetArchived(archive_id.clone(), !archived));
                    });
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(rust_i18n::t!("sidebar.delete_session")).on_click(
                    move |_, _, cx| {
                        let _ = delete_view.update(cx, |_, cx| {
                            cx.emit(SidebarEvent::DeleteSession(delete_id.clone()));
                        });
                    },
                ),
            )
        }
    }

    /// Workspace row options menu (shared by the row-tail "..." button and
    /// right-click):
    /// copy path / rename / remove workspace (hidden from the sidebar, session data
    /// kept).
    pub(crate) fn workspace_menu(
        view: &WeakEntity<Self>,
        path: &str,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + Clone + 'static
    {
        let view = view.clone();
        let path = path.to_string();
        move |menu, _, _| {
            let copy_path = path.clone();
            let rename_view = view.clone();
            let rename_path = path.clone();
            let remove_view = view.clone();
            let remove_path = path.clone();
            menu.item(
                PopupMenuItem::new(rust_i18n::t!("sidebar.copy_path")).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                }),
            )
            .item(
                PopupMenuItem::new(rust_i18n::t!("sidebar.rename")).on_click(
                    move |_, window, cx| {
                        let _ = rename_view.update(cx, |this, cx| {
                            this.start_rename(rename_path.clone(), window, cx);
                        });
                    },
                ),
            )
            .separator()
            .item(
                PopupMenuItem::new(rust_i18n::t!("sidebar.remove_workspace")).on_click(
                    move |_, _, cx| {
                        let _ = remove_view.update(cx, |_, cx| {
                            cx.emit(SidebarEvent::RemoveWorkspace(remove_path.clone()));
                        });
                    },
                ),
            )
        }
    }
}
