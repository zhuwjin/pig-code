use super::*;

impl Sidebar {
    /// 头部「列表管理」菜单：视图二选一（平铺列表 / 按工作区分组），
    /// 勾选标记在右侧（对齐 ZCode）
    pub(crate) fn view_menu(
        view: &WeakEntity<Self>,
        current: SidebarView,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + Clone + 'static
    {
        let view = view.clone();
        move |menu, _, _| {
            let item = |label: &'static str, icon: AssetsIconName, target: SidebarView| {
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
            menu.label("视图")
                .item(item("平铺列表", AssetsIconName::List, SidebarView::Flat))
                .item(item(
                    "按工作区分组",
                    AssetsIconName::FolderKanban,
                    SidebarView::Workspace,
                ))
                .check_side(Side::Right)
        }
    }

    /// 会话行的右键菜单：重命名 / 置顶 / 归档 / 删除（清库+rollout，不可恢复）
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
            menu.item(PopupMenuItem::new("重命名").on_click(move |_, window, cx| {
                let _ = rename_view.update(cx, |this, cx| {
                    this.start_session_rename(rename_id.clone(), rename_title.clone(), window, cx);
                });
            }))
            .item(
                PopupMenuItem::new(if pinned { "取消置顶" } else { "置顶" }).on_click(
                    move |_, _, cx| {
                        let _ = pin_view.update(cx, |_, cx| {
                            cx.emit(SidebarEvent::SetPinned(pin_id.clone(), !pinned));
                        });
                    },
                ),
            )
            .item(
                PopupMenuItem::new(if archived { "取消归档" } else { "归档" }).on_click(
                    move |_, _, cx| {
                        let _ = archive_view.update(cx, |_, cx| {
                            cx.emit(SidebarEvent::SetArchived(archive_id.clone(), !archived));
                        });
                    },
                ),
            )
            .separator()
            .item(PopupMenuItem::new("删除会话").on_click(move |_, _, cx| {
                let _ = delete_view.update(cx, |_, cx| {
                    cx.emit(SidebarEvent::DeleteSession(delete_id.clone()));
                });
            }))
        }
    }

    /// 工作区行的选项菜单（行尾 “...” 按钮与右键共用）：
    /// 复制路径 / 重命名 / 移除工作区（从侧栏隐藏，会话数据保留）。
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
            menu.item(PopupMenuItem::new("复制路径").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
            }))
            .item(PopupMenuItem::new("重命名").on_click(move |_, window, cx| {
                let _ = rename_view.update(cx, |this, cx| {
                    this.start_rename(rename_path.clone(), window, cx);
                });
            }))
            .separator()
            .item(PopupMenuItem::new("移除工作区").on_click(move |_, _, cx| {
                let _ = remove_view.update(cx, |_, cx| {
                    cx.emit(SidebarEvent::RemoveWorkspace(remove_path.clone()));
                });
            }))
        }
    }
}
