use crate::{
    db::{self, NewUserParams, UserId},
    rpc::{CLEANUP_TIMEOUT, RECONNECT_TIMEOUT},
    tests::{TestClient, TestServer},
};
use anyhow::{anyhow, Result};
use call::ActiveCall;
use client::RECEIVE_TIMEOUT;
use collections::BTreeMap;
use editor::Bias;
use fs::{repository::GitFileStatus, FakeFs, Fs as _};
use futures::StreamExt as _;
use gpui::{executor::Deterministic, ModelHandle, Task, TestAppContext};
use language::{range_to_lsp, FakeLspAdapter, Language, LanguageConfig, PointUtf16};
use lsp::FakeLanguageServer;
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use project::{search::SearchQuery, Project, ProjectPath};
use rand::{
    distributions::{Alphanumeric, DistString},
    prelude::*,
};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use std::{
    env,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering::SeqCst},
        Arc,
    },
};
use util::ResultExt;

lazy_static::lazy_static! {
    static ref PLAN_LOAD_PATH: Option<PathBuf> = path_env_var("LOAD_PLAN");
    static ref PLAN_SAVE_PATH: Option<PathBuf> = path_env_var("SAVE_PLAN");
    static ref LOADED_PLAN_JSON: Mutex<Option<Vec<u8>>> = Default::default();
    static ref PLAN: Mutex<Option<Arc<Mutex<TestPlan>>>> = Default::default();
}

#[gpui::test(iterations = 100, on_failure = "on_failure")]
async fn test_random_collaboration(
    cx: &mut TestAppContext,
    deterministic: Arc<Deterministic>,
    rng: StdRng,
) {
    deterministic.forbid_parking();

    let max_peers = env::var("MAX_PEERS")
        .map(|i| i.parse().expect("invalid `MAX_PEERS` variable"))
        .unwrap_or(3);
    let max_operations = env::var("OPERATIONS")
        .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
        .unwrap_or(10);

    let mut server = TestServer::start(&deterministic).await;
    let db = server.app_state.db.clone();

    let mut users = Vec::new();
    for ix in 0..max_peers {
        let username = format!("user-{}", ix + 1);
        let user_id = db
            .create_user(
                &format!("{username}@example.com"),
                false,
                NewUserParams {
                    github_login: username.clone(),
                    github_user_id: (ix + 1) as i32,
                    invite_count: 0,
                },
            )
            .await
            .unwrap()
            .user_id;
        users.push(UserTestPlan {
            user_id,
            username,
            online: false,
            next_root_id: 0,
            operation_ix: 0,
        });
    }

    for (ix, user_a) in users.iter().enumerate() {
        for user_b in &users[ix + 1..] {
            server
                .app_state
                .db
                .send_contact_request(user_a.user_id, user_b.user_id)
                .await
                .unwrap();
            server
                .app_state
                .db
                .respond_to_contact_request(user_b.user_id, user_a.user_id, true)
                .await
                .unwrap();
        }
    }

    let plan = Arc::new(Mutex::new(TestPlan::new(rng, users, max_operations)));

    if let Some(path) = &*PLAN_LOAD_PATH {
        let json = LOADED_PLAN_JSON
            .lock()
            .get_or_insert_with(|| {
                eprintln!("loaded test plan from path {:?}", path);
                std::fs::read(path).unwrap()
            })
            .clone();
        plan.lock().deserialize(json);
    }

    PLAN.lock().replace(plan.clone());

    let mut clients = Vec::new();
    let mut client_tasks = Vec::new();
    let mut operation_channels = Vec::new();

    loop {
        let Some((next_operation, applied)) = plan.lock().next_server_operation(&clients) else { break };
        applied.store(true, SeqCst);
        let did_apply = apply_server_operation(
            deterministic.clone(),
            &mut server,
            &mut clients,
            &mut client_tasks,
            &mut operation_channels,
            plan.clone(),
            next_operation,
            cx,
        )
        .await;
        if !did_apply {
            applied.store(false, SeqCst);
        }
    }

    drop(operation_channels);
    deterministic.start_waiting();
    futures::future::join_all(client_tasks).await;
    deterministic.finish_waiting();
    deterministic.run_until_parked();

    check_consistency_between_clients(&clients);

    for (client, mut cx) in clients {
        cx.update(|cx| {
            let store = cx.remove_global::<SettingsStore>();
            let settings = cx.remove_global::<Settings>();
            cx.clear_globals();
            cx.set_global(store);
            cx.set_global(settings);
            drop(client);
        });
    }

    deterministic.run_until_parked();
}

fn on_failure() {
    if let Some(plan) = PLAN.lock().clone() {
        if let Some(path) = &*PLAN_SAVE_PATH {
            eprintln!("saved test plan to path {:?}", path);
            std::fs::write(path, plan.lock().serialize()).unwrap();
        }
    }
}

async fn apply_server_operation(
    deterministic: Arc<Deterministic>,
    server: &mut TestServer,
    clients: &mut Vec<(Rc<TestClient>, TestAppContext)>,
    client_tasks: &mut Vec<Task<()>>,
    operation_channels: &mut Vec<futures::channel::mpsc::UnboundedSender<usize>>,
    plan: Arc<Mutex<TestPlan>>,
    operation: Operation,
    cx: &mut TestAppContext,
) -> bool {
    match operation {
        Operation::AddConnection { user_id } => {
            let username;
            {
                let mut plan = plan.lock();
                let mut user = plan.user(user_id);
                if user.online {
                    return false;
                }
                user.online = true;
                username = user.username.clone();
            };
            log::info!("Adding new connection for {}", username);
            let next_entity_id = (user_id.0 * 10_000) as usize;
            let mut client_cx = TestAppContext::new(
                cx.foreground_platform(),
                cx.platform(),
                deterministic.build_foreground(user_id.0 as usize),
                deterministic.build_background(),
                cx.font_cache(),
                cx.leak_detector(),
                next_entity_id,
                cx.function_name.clone(),
            );

            let (operation_tx, operation_rx) = futures::channel::mpsc::unbounded();
            let client = Rc::new(server.create_client(&mut client_cx, &username).await);
            operation_channels.push(operation_tx);
            clients.push((client.clone(), client_cx.clone()));
            client_tasks.push(client_cx.foreground().spawn(simulate_client(
                client,
                operation_rx,
                plan.clone(),
                client_cx,
            )));

            log::info!("Added connection for {}", username);
        }

        Operation::RemoveConnection {
            user_id: removed_user_id,
        } => {
            log::info!("Simulating full disconnection of user {}", removed_user_id);
            let client_ix = clients
                .iter()
                .position(|(client, cx)| client.current_user_id(cx) == removed_user_id);
            let Some(client_ix) = client_ix else { return false };
            let user_connection_ids = server
                .connection_pool
                .lock()
                .user_connection_ids(removed_user_id)
                .collect::<Vec<_>>();
            assert_eq!(user_connection_ids.len(), 1);
            let removed_peer_id = user_connection_ids[0].into();
            let (client, mut client_cx) = clients.remove(client_ix);
            let client_task = client_tasks.remove(client_ix);
            operation_channels.remove(client_ix);
            server.forbid_connections();
            server.disconnect_client(removed_peer_id);
            deterministic.advance_clock(RECEIVE_TIMEOUT + RECONNECT_TIMEOUT);
            deterministic.start_waiting();
            log::info!("Waiting for user {} to exit...", removed_user_id);
            client_task.await;
            deterministic.finish_waiting();
            server.allow_connections();

            for project in client.remote_projects().iter() {
                project.read_with(&client_cx, |project, _| {
                    assert!(
                        project.is_read_only(),
                        "project {:?} should be read only",
                        project.remote_id()
                    )
                });
            }

            for (client, cx) in clients {
                let contacts = server
                    .app_state
                    .db
                    .get_contacts(client.current_user_id(cx))
                    .await
                    .unwrap();
                let pool = server.connection_pool.lock();
                for contact in contacts {
                    if let db::Contact::Accepted { user_id, busy, .. } = contact {
                        if user_id == removed_user_id {
                            assert!(!pool.is_user_online(user_id));
                            assert!(!busy);
                        }
                    }
                }
            }

            log::info!("{} removed", client.username);
            plan.lock().user(removed_user_id).online = false;
            client_cx.update(|cx| {
                cx.clear_globals();
                drop(client);
            });
        }

        Operation::BounceConnection { user_id } => {
            log::info!("Simulating temporary disconnection of user {}", user_id);
            let user_connection_ids = server
                .connection_pool
                .lock()
                .user_connection_ids(user_id)
                .collect::<Vec<_>>();
            if user_connection_ids.is_empty() {
                return false;
            }
            assert_eq!(user_connection_ids.len(), 1);
            let peer_id = user_connection_ids[0].into();
            server.disconnect_client(peer_id);
            deterministic.advance_clock(RECEIVE_TIMEOUT + RECONNECT_TIMEOUT);
        }

        Operation::RestartServer => {
            log::info!("Simulating server restart");
            server.reset().await;
            deterministic.advance_clock(RECEIVE_TIMEOUT);
            server.start().await.unwrap();
            deterministic.advance_clock(CLEANUP_TIMEOUT);
            let environment = &server.app_state.config.zed_environment;
            let stale_room_ids = server
                .app_state
                .db
                .stale_room_ids(environment, server.id())
                .await
                .unwrap();
            assert_eq!(stale_room_ids, vec![]);
        }

        Operation::MutateClients {
            user_ids,
            batch_id,
            quiesce,
        } => {
            let mut applied = false;
            for user_id in user_ids {
                let client_ix = clients
                    .iter()
                    .position(|(client, cx)| client.current_user_id(cx) == user_id);
                let Some(client_ix) = client_ix else { continue };
                applied = true;
                if let Err(err) = operation_channels[client_ix].unbounded_send(batch_id) {
                    log::error!("error signaling user {user_id}: {err}");
                }
            }

            if quiesce && applied {
                deterministic.run_until_parked();
                check_consistency_between_clients(&clients);
            }

            return applied;
        }
    }
    true
}

async fn apply_client_operation(
    client: &TestClient,
    operation: ClientOperation,
    cx: &mut TestAppContext,
) -> Result<(), TestError> {
    match operation {
        ClientOperation::AcceptIncomingCall => {
            let active_call = cx.read(ActiveCall::global);
            if active_call.read_with(cx, |call, _| call.incoming().borrow().is_none()) {
                Err(TestError::Inapplicable)?;
            }

            log::info!("{}: accepting incoming call", client.username);
            active_call
                .update(cx, |call, cx| call.accept_incoming(cx))
                .await?;
        }

        ClientOperation::RejectIncomingCall => {
            let active_call = cx.read(ActiveCall::global);
            if active_call.read_with(cx, |call, _| call.incoming().borrow().is_none()) {
                Err(TestError::Inapplicable)?;
            }

            log::info!("{}: declining incoming call", client.username);
            active_call.update(cx, |call, _| call.decline_incoming())?;
        }

        ClientOperation::LeaveCall => {
            let active_call = cx.read(ActiveCall::global);
            if active_call.read_with(cx, |call, _| call.room().is_none()) {
                Err(TestError::Inapplicable)?;
            }

            log::info!("{}: hanging up", client.username);
            active_call.update(cx, |call, cx| call.hang_up(cx)).await?;
        }

        ClientOperation::InviteContactToCall { user_id } => {
            let active_call = cx.read(ActiveCall::global);

            log::info!("{}: inviting {}", client.username, user_id,);
            active_call
                .update(cx, |call, cx| call.invite(user_id.to_proto(), None, cx))
                .await
                .log_err();
        }

        ClientOperation::OpenLocalProject { first_root_name } => {
            log::info!(
                "{}: opening local project at {:?}",
                client.username,
                first_root_name
            );

            let root_path = Path::new("/").join(&first_root_name);
            client.fs.create_dir(&root_path).await.unwrap();
            client
                .fs
                .create_file(&root_path.join("main.rs"), Default::default())
                .await
                .unwrap();
            let project = client.build_local_project(root_path, cx).await.0;
            ensure_project_shared(&project, client, cx).await;
            client.local_projects_mut().push(project.clone());
        }

        ClientOperation::AddWorktreeToProject {
            project_root_name,
            new_root_path,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: finding/creating local worktree at {:?} to project with root path {}",
                client.username,
                new_root_path,
                project_root_name
            );

            ensure_project_shared(&project, client, cx).await;
            if !client.fs.paths().contains(&new_root_path) {
                client.fs.create_dir(&new_root_path).await.unwrap();
            }
            project
                .update(cx, |project, cx| {
                    project.find_or_create_local_worktree(&new_root_path, true, cx)
                })
                .await
                .unwrap();
        }

        ClientOperation::CloseRemoteProject { project_root_name } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: closing remote project with root path {}",
                client.username,
                project_root_name,
            );

            let ix = client
                .remote_projects()
                .iter()
                .position(|p| p == &project)
                .unwrap();
            cx.update(|_| {
                client.remote_projects_mut().remove(ix);
                client.buffers().retain(|p, _| *p != project);
                drop(project);
            });
        }

        ClientOperation::OpenRemoteProject {
            host_id,
            first_root_name,
        } => {
            let active_call = cx.read(ActiveCall::global);
            let project = active_call
                .update(cx, |call, cx| {
                    let room = call.room().cloned()?;
                    let participant = room
                        .read(cx)
                        .remote_participants()
                        .get(&host_id.to_proto())?;
                    let project_id = participant
                        .projects
                        .iter()
                        .find(|project| project.worktree_root_names[0] == first_root_name)?
                        .id;
                    Some(room.update(cx, |room, cx| {
                        room.join_project(
                            project_id,
                            client.language_registry.clone(),
                            FakeFs::new(cx.background().clone()),
                            cx,
                        )
                    }))
                })
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: joining remote project of user {}, root name {}",
                client.username,
                host_id,
                first_root_name,
            );

            let project = project.await?;
            client.remote_projects_mut().push(project.clone());
        }

        ClientOperation::CreateWorktreeEntry {
            project_root_name,
            is_local,
            full_path,
            is_dir,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;
            let project_path = project_path_for_full_path(&project, &full_path, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: creating {} at path {:?} in {} project {}",
                client.username,
                if is_dir { "dir" } else { "file" },
                full_path,
                if is_local { "local" } else { "remote" },
                project_root_name,
            );

            ensure_project_shared(&project, client, cx).await;
            project
                .update(cx, |p, cx| p.create_entry(project_path, is_dir, cx))
                .unwrap()
                .await?;
        }

        ClientOperation::OpenBuffer {
            project_root_name,
            is_local,
            full_path,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;
            let project_path = project_path_for_full_path(&project, &full_path, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: opening buffer {:?} in {} project {}",
                client.username,
                full_path,
                if is_local { "local" } else { "remote" },
                project_root_name,
            );

            ensure_project_shared(&project, client, cx).await;
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;
            client.buffers_for_project(&project).insert(buffer);
        }

        ClientOperation::EditBuffer {
            project_root_name,
            is_local,
            full_path,
            edits,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;
            let buffer = buffer_for_full_path(client, &project, &full_path, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: editing buffer {:?} in {} project {} with {:?}",
                client.username,
                full_path,
                if is_local { "local" } else { "remote" },
                project_root_name,
                edits
            );

            ensure_project_shared(&project, client, cx).await;
            buffer.update(cx, |buffer, cx| {
                let snapshot = buffer.snapshot();
                buffer.edit(
                    edits.into_iter().map(|(range, text)| {
                        let start = snapshot.clip_offset(range.start, Bias::Left);
                        let end = snapshot.clip_offset(range.end, Bias::Right);
                        (start..end, text)
                    }),
                    None,
                    cx,
                );
            });
        }

        ClientOperation::CloseBuffer {
            project_root_name,
            is_local,
            full_path,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;
            let buffer = buffer_for_full_path(client, &project, &full_path, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: closing buffer {:?} in {} project {}",
                client.username,
                full_path,
                if is_local { "local" } else { "remote" },
                project_root_name
            );

            ensure_project_shared(&project, client, cx).await;
            cx.update(|_| {
                client.buffers_for_project(&project).remove(&buffer);
                drop(buffer);
            });
        }

        ClientOperation::SaveBuffer {
            project_root_name,
            is_local,
            full_path,
            detach,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;
            let buffer = buffer_for_full_path(client, &project, &full_path, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: saving buffer {:?} in {} project {}, {}",
                client.username,
                full_path,
                if is_local { "local" } else { "remote" },
                project_root_name,
                if detach { "detaching" } else { "awaiting" }
            );

            ensure_project_shared(&project, client, cx).await;
            let requested_version = buffer.read_with(cx, |buffer, _| buffer.version());
            let save = project.update(cx, |project, cx| project.save_buffer(buffer, cx));
            let save = cx.background().spawn(async move {
                let (saved_version, _, _) = save
                    .await
                    .map_err(|err| anyhow!("save request failed: {:?}", err))?;
                assert!(saved_version.observed_all(&requested_version));
                anyhow::Ok(())
            });
            if detach {
                cx.update(|cx| save.detach_and_log_err(cx));
            } else {
                save.await?;
            }
        }

        ClientOperation::RequestLspDataInBuffer {
            project_root_name,
            is_local,
            full_path,
            offset,
            kind,
            detach,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;
            let buffer = buffer_for_full_path(client, &project, &full_path, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: request LSP {:?} for buffer {:?} in {} project {}, {}",
                client.username,
                kind,
                full_path,
                if is_local { "local" } else { "remote" },
                project_root_name,
                if detach { "detaching" } else { "awaiting" }
            );

            use futures::{FutureExt as _, TryFutureExt as _};
            let offset = buffer.read_with(cx, |b, _| b.clip_offset(offset, Bias::Left));
            let request = cx.foreground().spawn(project.update(cx, |project, cx| {
                match kind {
                    LspRequestKind::Rename => project
                        .prepare_rename(buffer, offset, cx)
                        .map_ok(|_| ())
                        .boxed(),
                    LspRequestKind::Completion => project
                        .completions(&buffer, offset, cx)
                        .map_ok(|_| ())
                        .boxed(),
                    LspRequestKind::CodeAction => project
                        .code_actions(&buffer, offset..offset, cx)
                        .map_ok(|_| ())
                        .boxed(),
                    LspRequestKind::Definition => project
                        .definition(&buffer, offset, cx)
                        .map_ok(|_| ())
                        .boxed(),
                    LspRequestKind::Highlights => project
                        .document_highlights(&buffer, offset, cx)
                        .map_ok(|_| ())
                        .boxed(),
                }
            }));
            if detach {
                request.detach();
            } else {
                request.await?;
            }
        }

        ClientOperation::SearchProject {
            project_root_name,
            is_local,
            query,
            detach,
        } => {
            let project = project_for_root_name(client, &project_root_name, cx)
                .ok_or(TestError::Inapplicable)?;

            log::info!(
                "{}: search {} project {} for {:?}, {}",
                client.username,
                if is_local { "local" } else { "remote" },
                project_root_name,
                query,
                if detach { "detaching" } else { "awaiting" }
            );

            let search = project.update(cx, |project, cx| {
                project.search(
                    SearchQuery::text(query, false, false, Vec::new(), Vec::new()),
                    cx,
                )
            });
            drop(project);
            let search = cx.background().spawn(async move {
                search
                    .await
                    .map_err(|err| anyhow!("search request failed: {:?}", err))
            });
            if detach {
                cx.update(|cx| search.detach_and_log_err(cx));
            } else {
                search.await?;
            }
        }

        ClientOperation::WriteFsEntry {
            path,
            is_dir,
            content,
        } => {
            if !client
                .fs
                .directories()
                .contains(&path.parent().unwrap().to_owned())
            {
                return Err(TestError::Inapplicable);
            }

            if is_dir {
                log::info!("{}: creating dir at {:?}", client.username, path);
                client.fs.create_dir(&path).await.unwrap();
            } else {
                let exists = client.fs.metadata(&path).await?.is_some();
                let verb = if exists { "updating" } else { "creating" };
                log::info!("{}: {} file at {:?}", verb, client.username, path);

                client
                    .fs
                    .save(&path, &content.as_str().into(), fs::LineEnding::Unix)
                    .await
                    .unwrap();
            }
        }

        ClientOperation::GitOperation { operation } => match operation {
            GitOperation::WriteGitIndex {
                repo_path,
                contents,
            } => {
                if !client.fs.directories().contains(&repo_path) {
                    return Err(TestError::Inapplicable);
                }

                log::info!(
                    "{}: writing git index for repo {:?}: {:?}",
                    client.username,
                    repo_path,
                    contents
                );

                let dot_git_dir = repo_path.join(".git");
                let contents = contents
                    .iter()
                    .map(|(path, contents)| (path.as_path(), contents.clone()))
                    .collect::<Vec<_>>();
                if client.fs.metadata(&dot_git_dir).await?.is_none() {
                    client.fs.create_dir(&dot_git_dir).await?;
                }
                client.fs.set_index_for_repo(&dot_git_dir, &contents).await;
            }
            GitOperation::WriteGitBranch {
                repo_path,
                new_branch,
            } => {
                if !client.fs.directories().contains(&repo_path) {
                    return Err(TestError::Inapplicable);
                }

                log::info!(
                    "{}: writing git branch for repo {:?}: {:?}",
                    client.username,
                    repo_path,
                    new_branch
                );

                let dot_git_dir = repo_path.join(".git");
                if client.fs.metadata(&dot_git_dir).await?.is_none() {
                    client.fs.create_dir(&dot_git_dir).await?;
                }
                client.fs.set_branch_name(&dot_git_dir, new_branch).await;
            }
            GitOperation::WriteGitStatuses {
                repo_path,
                statuses,
            } => {
                if !client.fs.directories().contains(&repo_path) {
                    return Err(TestError::Inapplicable);
                }

                log::info!(
                    "{}: writing git statuses for repo {:?}: {:?}",
                    client.username,
                    repo_path,
                    statuses
                );

                let dot_git_dir = repo_path.join(".git");

                let statuses = statuses
                    .iter()
                    .map(|(path, val)| (path.as_path(), val.clone()))
                    .collect::<Vec<_>>();

                if client.fs.metadata(&dot_git_dir).await?.is_none() {
                    client.fs.create_dir(&dot_git_dir).await?;
                }

                client
                    .fs
                    .set_status_for_repo(&dot_git_dir, statuses.as_slice())
                    .await;
            }
        },
    }
    Ok(())
}

fn check_consistency_between_clients(clients: &[(Rc<TestClient>, TestAppContext)]) {
    for (client, client_cx) in clients {
        for guest_project in client.remote_projects().iter() {
            guest_project.read_with(client_cx, |guest_project, cx| {
                let host_project = clients.iter().find_map(|(client, cx)| {
                    let project = client
                        .local_projects()
                        .iter()
                        .find(|host_project| {
                            host_project.read_with(cx, |host_project, _| {
                                host_project.remote_id() == guest_project.remote_id()
                            })
                        })?
                        .clone();
                    Some((project, cx))
                });

                if !guest_project.is_read_only() {
                    if let Some((host_project, host_cx)) = host_project {
                        let host_worktree_snapshots =
                            host_project.read_with(host_cx, |host_project, cx| {
                                host_project
                                    .worktrees(cx)
                                    .map(|worktree| {
                                        let worktree = worktree.read(cx);
                                        (worktree.id(), worktree.snapshot())
                                    })
                                    .collect::<BTreeMap<_, _>>()
                            });
                        let guest_worktree_snapshots = guest_project
                            .worktrees(cx)
                            .map(|worktree| {
                                let worktree = worktree.read(cx);
                                (worktree.id(), worktree.snapshot())
                            })
                            .collect::<BTreeMap<_, _>>();

                        assert_eq!(
                            guest_worktree_snapshots.values().map(|w| w.abs_path()).collect::<Vec<_>>(),
                            host_worktree_snapshots.values().map(|w| w.abs_path()).collect::<Vec<_>>(),
                            "{} has different worktrees than the host for project {:?}",
                            client.username, guest_project.remote_id(),
                        );

                        for (id, host_snapshot) in &host_worktree_snapshots {
                            let guest_snapshot = &guest_worktree_snapshots[id];
                            assert_eq!(
                                guest_snapshot.root_name(),
                                host_snapshot.root_name(),
                                "{} has different root name than the host for worktree {}, project {:?}",
                                client.username,
                                id,
                                guest_project.remote_id(),
                            );
                            assert_eq!(
                                guest_snapshot.abs_path(),
                                host_snapshot.abs_path(),
                                "{} has different abs path than the host for worktree {}, project: {:?}",
                                client.username,
                                id,
                                guest_project.remote_id(),
                            );
                            assert_eq!(
                                guest_snapshot.entries(false).collect::<Vec<_>>(),
                                host_snapshot.entries(false).collect::<Vec<_>>(),
                                "{} has different snapshot than the host for worktree {:?} and project {:?}",
                                client.username,
                                host_snapshot.abs_path(),
                                guest_project.remote_id(),
                            );
                            assert_eq!(guest_snapshot.repositories().collect::<Vec<_>>(), host_snapshot.repositories().collect::<Vec<_>>(),
                                "{} has different repositories than the host for worktree {:?} and project {:?}",
                                client.username,
                                host_snapshot.abs_path(),
                                guest_project.remote_id(),
                            );
                            assert_eq!(guest_snapshot.scan_id(), host_snapshot.scan_id(),
                                "{} has different scan id than the host for worktree {:?} and project {:?}",
                                client.username,
                                host_snapshot.abs_path(),
                                guest_project.remote_id(),
                            );
                        }
                    }
                }

                for buffer in guest_project.opened_buffers(cx) {
                    let buffer = buffer.read(cx);
                    assert_eq!(
                        buffer.deferred_ops_len(),
                        0,
                        "{} has deferred operations for buffer {:?} in project {:?}",
                        client.username,
                        buffer.file().unwrap().full_path(cx),
                        guest_project.remote_id(),
                    );
                }
            });
        }

        let buffers = client.buffers().clone();
        for (guest_project, guest_buffers) in &buffers {
            let project_id = if guest_project.read_with(client_cx, |project, _| {
                project.is_local() || project.is_read_only()
            }) {
                continue;
            } else {
                guest_project
                    .read_with(client_cx, |project, _| project.remote_id())
                    .unwrap()
            };
            let guest_user_id = client.user_id().unwrap();

            let host_project = clients.iter().find_map(|(client, cx)| {
                let project = client
                    .local_projects()
                    .iter()
                    .find(|host_project| {
                        host_project.read_with(cx, |host_project, _| {
                            host_project.remote_id() == Some(project_id)
                        })
                    })?
                    .clone();
                Some((client.user_id().unwrap(), project, cx))
            });

            let (host_user_id, host_project, host_cx) =
                if let Some((host_user_id, host_project, host_cx)) = host_project {
                    (host_user_id, host_project, host_cx)
                } else {
                    continue;
                };

            for guest_buffer in guest_buffers {
                let buffer_id = guest_buffer.read_with(client_cx, |buffer, _| buffer.remote_id());
                let host_buffer = host_project.read_with(host_cx, |project, cx| {
                    project.buffer_for_id(buffer_id, cx).unwrap_or_else(|| {
                        panic!(
                            "host does not have buffer for guest:{}, peer:{:?}, id:{}",
                            client.username,
                            client.peer_id(),
                            buffer_id
                        )
                    })
                });
                let path = host_buffer
                    .read_with(host_cx, |buffer, cx| buffer.file().unwrap().full_path(cx));

                assert_eq!(
                    guest_buffer.read_with(client_cx, |buffer, _| buffer.deferred_ops_len()),
                    0,
                    "{}, buffer {}, path {:?} has deferred operations",
                    client.username,
                    buffer_id,
                    path,
                );
                assert_eq!(
                    guest_buffer.read_with(client_cx, |buffer, _| buffer.text()),
                    host_buffer.read_with(host_cx, |buffer, _| buffer.text()),
                    "{}, buffer {}, path {:?}, differs from the host's buffer",
                    client.username,
                    buffer_id,
                    path
                );

                let host_file = host_buffer.read_with(host_cx, |b, _| b.file().cloned());
                let guest_file = guest_buffer.read_with(client_cx, |b, _| b.file().cloned());
                match (host_file, guest_file) {
                    (Some(host_file), Some(guest_file)) => {
                        assert_eq!(guest_file.path(), host_file.path());
                        assert_eq!(guest_file.is_deleted(), host_file.is_deleted());
                        assert_eq!(
                            guest_file.mtime(),
                            host_file.mtime(),
                            "guest {} mtime does not match host {} for path {:?} in project {}",
                            guest_user_id,
                            host_user_id,
                            guest_file.path(),
                            project_id,
                        );
                    }
                    (None, None) => {}
                    (None, _) => panic!("host's file is None, guest's isn't"),
                    (_, None) => panic!("guest's file is None, hosts's isn't"),
                }

                let host_diff_base =
                    host_buffer.read_with(host_cx, |b, _| b.diff_base().map(ToString::to_string));
                let guest_diff_base = guest_buffer
                    .read_with(client_cx, |b, _| b.diff_base().map(ToString::to_string));
                assert_eq!(
                    guest_diff_base, host_diff_base,
                    "guest {} diff base does not match host's for path {path:?} in project {project_id}",
                    client.username
                );

                let host_saved_version =
                    host_buffer.read_with(host_cx, |b, _| b.saved_version().clone());
                let guest_saved_version =
                    guest_buffer.read_with(client_cx, |b, _| b.saved_version().clone());
                assert_eq!(
                    guest_saved_version, host_saved_version,
                    "guest {} saved version does not match host's for path {path:?} in project {project_id}",
                    client.username
                );

                let host_saved_version_fingerprint =
                    host_buffer.read_with(host_cx, |b, _| b.saved_version_fingerprint());
                let guest_saved_version_fingerprint =
                    guest_buffer.read_with(client_cx, |b, _| b.saved_version_fingerprint());
                assert_eq!(
                    guest_saved_version_fingerprint, host_saved_version_fingerprint,
                    "guest {} saved fingerprint does not match host's for path {path:?} in project {project_id}",
                    client.username
                );

                let host_saved_mtime = host_buffer.read_with(host_cx, |b, _| b.saved_mtime());
                let guest_saved_mtime = guest_buffer.read_with(client_cx, |b, _| b.saved_mtime());
                assert_eq!(
                    guest_saved_mtime, host_saved_mtime,
                    "guest {} saved mtime does not match host's for path {path:?} in project {project_id}",
                    client.username
                );

                let host_is_dirty = host_buffer.read_with(host_cx, |b, _| b.is_dirty());
                let guest_is_dirty = guest_buffer.read_with(client_cx, |b, _| b.is_dirty());
                assert_eq!(guest_is_dirty, host_is_dirty,
                    "guest {} dirty status does not match host's for path {path:?} in project {project_id}",
                    client.username
                );

                let host_has_conflict = host_buffer.read_with(host_cx, |b, _| b.has_conflict());
                let guest_has_conflict = guest_buffer.read_with(client_cx, |b, _| b.has_conflict());
                assert_eq!(guest_has_conflict, host_has_conflict,
                    "guest {} conflict status does not match host's for path {path:?} in project {project_id}",
                    client.username
                );
            }
        }
    }
}

struct TestPlan {
    rng: StdRng,
    replay: bool,
    stored_operations: Vec<(StoredOperation, Arc<AtomicBool>)>,
    max_operations: usize,
    operation_ix: usize,
    users: Vec<UserTestPlan>,
    next_batch_id: usize,
    allow_server_restarts: bool,
    allow_client_reconnection: bool,
    allow_client_disconnection: bool,
}

struct UserTestPlan {
    user_id: UserId,
    username: String,
    next_root_id: usize,
    operation_ix: usize,
    online: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredOperation {
    Server(Operation),
    Client {
        user_id: UserId,
        batch_id: usize,
        operation: ClientOperation,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Operation {
    AddConnection {
        user_id: UserId,
    },
    RemoveConnection {
        user_id: UserId,
    },
    BounceConnection {
        user_id: UserId,
    },
    RestartServer,
    MutateClients {
        batch_id: usize,
        #[serde(skip_serializing)]
        #[serde(skip_deserializing)]
        user_ids: Vec<UserId>,
        quiesce: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum ClientOperation {
    AcceptIncomingCall,
    RejectIncomingCall,
    LeaveCall,
    InviteContactToCall {
        user_id: UserId,
    },
    OpenLocalProject {
        first_root_name: String,
    },
    OpenRemoteProject {
        host_id: UserId,
        first_root_name: String,
    },
    AddWorktreeToProject {
        project_root_name: String,
        new_root_path: PathBuf,
    },
    CloseRemoteProject {
        project_root_name: String,
    },
    OpenBuffer {
        project_root_name: String,
        is_local: bool,
        full_path: PathBuf,
    },
    SearchProject {
        project_root_name: String,
        is_local: bool,
        query: String,
        detach: bool,
    },
    EditBuffer {
        project_root_name: String,
        is_local: bool,
        full_path: PathBuf,
        edits: Vec<(Range<usize>, Arc<str>)>,
    },
    CloseBuffer {
        project_root_name: String,
        is_local: bool,
        full_path: PathBuf,
    },
    SaveBuffer {
        project_root_name: String,
        is_local: bool,
        full_path: PathBuf,
        detach: bool,
    },
    RequestLspDataInBuffer {
        project_root_name: String,
        is_local: bool,
        full_path: PathBuf,
        offset: usize,
        kind: LspRequestKind,
        detach: bool,
    },
    CreateWorktreeEntry {
        project_root_name: String,
        is_local: bool,
        full_path: PathBuf,
        is_dir: bool,
    },
    WriteFsEntry {
        path: PathBuf,
        is_dir: bool,
        content: String,
    },
    GitOperation {
        operation: GitOperation,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum GitOperation {
    WriteGitIndex {
        repo_path: PathBuf,
        contents: Vec<(PathBuf, String)>,
    },
    WriteGitBranch {
        repo_path: PathBuf,
        new_branch: Option<String>,
    },
    WriteGitStatuses {
        repo_path: PathBuf,
        statuses: Vec<(PathBuf, GitFileStatus)>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum LspRequestKind {
    Rename,
    Completion,
    CodeAction,
    Definition,
    Highlights,
}

enum TestError {
    Inapplicable,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for TestError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value)
    }
}

impl TestPlan {
    fn new(mut rng: StdRng, users: Vec<UserTestPlan>, max_operations: usize) -> Self {
        Self {
            replay: false,
            allow_server_restarts: rng.gen_bool(0.7),
            allow_client_reconnection: rng.gen_bool(0.7),
            allow_client_disconnection: rng.gen_bool(0.1),
            stored_operations: Vec::new(),
            operation_ix: 0,
            next_batch_id: 0,
            max_operations,
            users,
            rng,
        }
    }

    fn deserialize(&mut self, json: Vec<u8>) {
        let stored_operations: Vec<StoredOperation> = serde_json::from_slice(&json).unwrap();
        self.replay = true;
        self.stored_operations = stored_operations
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, mut operation)| {
                if let StoredOperation::Server(Operation::MutateClients {
                    batch_id: current_batch_id,
                    user_ids,
                    ..
                }) = &mut operation
                {
                    assert!(user_ids.is_empty());
                    user_ids.extend(stored_operations[i + 1..].iter().filter_map(|operation| {
                        if let StoredOperation::Client {
                            user_id, batch_id, ..
                        } = operation
                        {
                            if batch_id == current_batch_id {
                                return Some(user_id);
                            }
                        }
                        None
                    }));
                    user_ids.sort_unstable();
                }
                (operation, Arc::new(AtomicBool::new(false)))
            })
            .collect()
    }

    fn serialize(&mut self) -> Vec<u8> {
        // Format each operation as one line
        let mut json = Vec::new();
        json.push(b'[');
        for (operation, applied) in &self.stored_operations {
            if !applied.load(SeqCst) {
                continue;
            }
            if json.len() > 1 {
                json.push(b',');
            }
            json.extend_from_slice(b"\n  ");
            serde_json::to_writer(&mut json, operation).unwrap();
        }
        json.extend_from_slice(b"\n]\n");
        json
    }

    fn next_server_operation(
        &mut self,
        clients: &[(Rc<TestClient>, TestAppContext)],
    ) -> Option<(Operation, Arc<AtomicBool>)> {
        if self.replay {
            while let Some(stored_operation) = self.stored_operations.get(self.operation_ix) {
                self.operation_ix += 1;
                if let (StoredOperation::Server(operation), applied) = stored_operation {
                    return Some((operation.clone(), applied.clone()));
                }
            }
            None
        } else {
            let operation = self.generate_server_operation(clients)?;
            let applied = Arc::new(AtomicBool::new(false));
            self.stored_operations
                .push((StoredOperation::Server(operation.clone()), applied.clone()));
            Some((operation, applied))
        }
    }

    fn next_client_operation(
        &mut self,
        client: &TestClient,
        current_batch_id: usize,
        cx: &TestAppContext,
    ) -> Option<(ClientOperation, Arc<AtomicBool>)> {
        let current_user_id = client.current_user_id(cx);
        let user_ix = self
            .users
            .iter()
            .position(|user| user.user_id == current_user_id)
            .unwrap();
        let user_plan = &mut self.users[user_ix];

        if self.replay {
            while let Some(stored_operation) = self.stored_operations.get(user_plan.operation_ix) {
                user_plan.operation_ix += 1;
                if let (
                    StoredOperation::Client {
                        user_id, operation, ..
                    },
                    applied,
                ) = stored_operation
                {
                    if user_id == &current_user_id {
                        return Some((operation.clone(), applied.clone()));
                    }
                }
            }
            None
        } else {
            let operation = self.generate_client_operation(current_user_id, client, cx)?;
            let applied = Arc::new(AtomicBool::new(false));
            self.stored_operations.push((
                StoredOperation::Client {
                    user_id: current_user_id,
                    batch_id: current_batch_id,
                    operation: operation.clone(),
                },
                applied.clone(),
            ));
            Some((operation, applied))
        }
    }

    fn generate_server_operation(
        &mut self,
        clients: &[(Rc<TestClient>, TestAppContext)],
    ) -> Option<Operation> {
        if self.operation_ix == self.max_operations {
            return None;
        }

        Some(loop {
            break match self.rng.gen_range(0..100) {
                0..=29 if clients.len() < self.users.len() => {
                    let user = self
                        .users
                        .iter()
                        .filter(|u| !u.online)
                        .choose(&mut self.rng)
                        .unwrap();
                    self.operation_ix += 1;
                    Operation::AddConnection {
                        user_id: user.user_id,
                    }
                }
                30..=34 if clients.len() > 1 && self.allow_client_disconnection => {
                    let (client, cx) = &clients[self.rng.gen_range(0..clients.len())];
                    let user_id = client.current_user_id(cx);
                    self.operation_ix += 1;
                    Operation::RemoveConnection { user_id }
                }
                35..=39 if clients.len() > 1 && self.allow_client_reconnection => {
                    let (client, cx) = &clients[self.rng.gen_range(0..clients.len())];
                    let user_id = client.current_user_id(cx);
                    self.operation_ix += 1;
                    Operation::BounceConnection { user_id }
                }
                40..=44 if self.allow_server_restarts && clients.len() > 1 => {
                    self.operation_ix += 1;
                    Operation::RestartServer
                }
                _ if !clients.is_empty() => {
                    let count = self
                        .rng
                        .gen_range(1..10)
                        .min(self.max_operations - self.operation_ix);
                    let batch_id = util::post_inc(&mut self.next_batch_id);
                    let mut user_ids = (0..count)
                        .map(|_| {
                            let ix = self.rng.gen_range(0..clients.len());
                            let (client, cx) = &clients[ix];
                            client.current_user_id(cx)
                        })
                        .collect::<Vec<_>>();
                    user_ids.sort_unstable();
                    Operation::MutateClients {
                        user_ids,
                        batch_id,
                        quiesce: self.rng.gen_bool(0.7),
                    }
                }
                _ => continue,
            };
        })
    }

    fn generate_client_operation(
        &mut self,
        user_id: UserId,
        client: &TestClient,
        cx: &TestAppContext,
    ) -> Option<ClientOperation> {
        if self.operation_ix == self.max_operations {
            return None;
        }

        self.operation_ix += 1;
        let call = cx.read(ActiveCall::global);
        Some(loop {
            match self.rng.gen_range(0..100_u32) {
                // Mutate the call
                0..=29 => {
                    // Respond to an incoming call
                    if call.read_with(cx, |call, _| call.incoming().borrow().is_some()) {
                        break if self.rng.gen_bool(0.7) {
                            ClientOperation::AcceptIncomingCall
                        } else {
                            ClientOperation::RejectIncomingCall
                        };
                    }

                    match self.rng.gen_range(0..100_u32) {
                        // Invite a contact to the current call
                        0..=70 => {
                            let available_contacts =
                                client.user_store.read_with(cx, |user_store, _| {
                                    user_store
                                        .contacts()
                                        .iter()
                                        .filter(|contact| contact.online && !contact.busy)
                                        .cloned()
                                        .collect::<Vec<_>>()
                                });
                            if !available_contacts.is_empty() {
                                let contact = available_contacts.choose(&mut self.rng).unwrap();
                                break ClientOperation::InviteContactToCall {
                                    user_id: UserId(contact.user.id as i32),
                                };
                            }
                        }

                        // Leave the current call
                        71.. => {
                            if self.allow_client_disconnection
                                && call.read_with(cx, |call, _| call.room().is_some())
                            {
                                break ClientOperation::LeaveCall;
                            }
                        }
                    }
                }

                // Mutate projects
                30..=59 => match self.rng.gen_range(0..100_u32) {
                    // Open a new project
                    0..=70 => {
                        // Open a remote project
                        if let Some(room) = call.read_with(cx, |call, _| call.room().cloned()) {
                            let existing_remote_project_ids = cx.read(|cx| {
                                client
                                    .remote_projects()
                                    .iter()
                                    .map(|p| p.read(cx).remote_id().unwrap())
                                    .collect::<Vec<_>>()
                            });
                            let new_remote_projects = room.read_with(cx, |room, _| {
                                room.remote_participants()
                                    .values()
                                    .flat_map(|participant| {
                                        participant.projects.iter().filter_map(|project| {
                                            if existing_remote_project_ids.contains(&project.id) {
                                                None
                                            } else {
                                                Some((
                                                    UserId::from_proto(participant.user.id),
                                                    project.worktree_root_names[0].clone(),
                                                ))
                                            }
                                        })
                                    })
                                    .collect::<Vec<_>>()
                            });
                            if !new_remote_projects.is_empty() {
                                let (host_id, first_root_name) =
                                    new_remote_projects.choose(&mut self.rng).unwrap().clone();
                                break ClientOperation::OpenRemoteProject {
                                    host_id,
                                    first_root_name,
                                };
                            }
                        }
                        // Open a local project
                        else {
                            let first_root_name = self.next_root_dir_name(user_id);
                            break ClientOperation::OpenLocalProject { first_root_name };
                        }
                    }

                    // Close a remote project
                    71..=80 => {
                        if !client.remote_projects().is_empty() {
                            let project = client
                                .remote_projects()
                                .choose(&mut self.rng)
                                .unwrap()
                                .clone();
                            let first_root_name = root_name_for_project(&project, cx);
                            break ClientOperation::CloseRemoteProject {
                                project_root_name: first_root_name,
                            };
                        }
                    }

                    // Mutate project worktrees
                    81.. => match self.rng.gen_range(0..100_u32) {
                        // Add a worktree to a local project
                        0..=50 => {
                            let Some(project) = client
                                .local_projects()
                                .choose(&mut self.rng)
                                .cloned() else { continue };
                            let project_root_name = root_name_for_project(&project, cx);
                            let mut paths = client.fs.paths();
                            paths.remove(0);
                            let new_root_path = if paths.is_empty() || self.rng.gen() {
                                Path::new("/").join(&self.next_root_dir_name(user_id))
                            } else {
                                paths.choose(&mut self.rng).unwrap().clone()
                            };
                            break ClientOperation::AddWorktreeToProject {
                                project_root_name,
                                new_root_path,
                            };
                        }

                        // Add an entry to a worktree
                        _ => {
                            let Some(project) = choose_random_project(client, &mut self.rng) else { continue };
                            let project_root_name = root_name_for_project(&project, cx);
                            let is_local = project.read_with(cx, |project, _| project.is_local());
                            let worktree = project.read_with(cx, |project, cx| {
                                project
                                    .worktrees(cx)
                                    .filter(|worktree| {
                                        let worktree = worktree.read(cx);
                                        worktree.is_visible()
                                            && worktree.entries(false).any(|e| e.is_file())
                                            && worktree.root_entry().map_or(false, |e| e.is_dir())
                                    })
                                    .choose(&mut self.rng)
                            });
                            let Some(worktree) = worktree else { continue };
                            let is_dir = self.rng.gen::<bool>();
                            let mut full_path =
                                worktree.read_with(cx, |w, _| PathBuf::from(w.root_name()));
                            full_path.push(gen_file_name(&mut self.rng));
                            if !is_dir {
                                full_path.set_extension("rs");
                            }
                            break ClientOperation::CreateWorktreeEntry {
                                project_root_name,
                                is_local,
                                full_path,
                                is_dir,
                            };
                        }
                    },
                },

                // Query and mutate buffers
                60..=90 => {
                    let Some(project) = choose_random_project(client, &mut self.rng) else { continue };
                    let project_root_name = root_name_for_project(&project, cx);
                    let is_local = project.read_with(cx, |project, _| project.is_local());

                    match self.rng.gen_range(0..100_u32) {
                        // Manipulate an existing buffer
                        0..=70 => {
                            let Some(buffer) = client
                                .buffers_for_project(&project)
                                .iter()
                                .choose(&mut self.rng)
                                .cloned() else { continue };

                            let full_path = buffer
                                .read_with(cx, |buffer, cx| buffer.file().unwrap().full_path(cx));

                            match self.rng.gen_range(0..100_u32) {
                                // Close the buffer
                                0..=15 => {
                                    break ClientOperation::CloseBuffer {
                                        project_root_name,
                                        is_local,
                                        full_path,
                                    };
                                }
                                // Save the buffer
                                16..=29 if buffer.read_with(cx, |b, _| b.is_dirty()) => {
                                    let detach = self.rng.gen_bool(0.3);
                                    break ClientOperation::SaveBuffer {
                                        project_root_name,
                                        is_local,
                                        full_path,
                                        detach,
                                    };
                                }
                                // Edit the buffer
                                30..=69 => {
                                    let edits = buffer.read_with(cx, |buffer, _| {
                                        buffer.get_random_edits(&mut self.rng, 3)
                                    });
                                    break ClientOperation::EditBuffer {
                                        project_root_name,
                                        is_local,
                                        full_path,
                                        edits,
                                    };
                                }
                                // Make an LSP request
                                _ => {
                                    let offset = buffer.read_with(cx, |buffer, _| {
                                        buffer.clip_offset(
                                            self.rng.gen_range(0..=buffer.len()),
                                            language::Bias::Left,
                                        )
                                    });
                                    let detach = self.rng.gen();
                                    break ClientOperation::RequestLspDataInBuffer {
                                        project_root_name,
                                        full_path,
                                        offset,
                                        is_local,
                                        kind: match self.rng.gen_range(0..5_u32) {
                                            0 => LspRequestKind::Rename,
                                            1 => LspRequestKind::Highlights,
                                            2 => LspRequestKind::Definition,
                                            3 => LspRequestKind::CodeAction,
                                            4.. => LspRequestKind::Completion,
                                        },
                                        detach,
                                    };
                                }
                            }
                        }

                        71..=80 => {
                            let query = self.rng.gen_range('a'..='z').to_string();
                            let detach = self.rng.gen_bool(0.3);
                            break ClientOperation::SearchProject {
                                project_root_name,
                                is_local,
                                query,
                                detach,
                            };
                        }

                        // Open a buffer
                        81.. => {
                            let worktree = project.read_with(cx, |project, cx| {
                                project
                                    .worktrees(cx)
                                    .filter(|worktree| {
                                        let worktree = worktree.read(cx);
                                        worktree.is_visible()
                                            && worktree.entries(false).any(|e| e.is_file())
                                    })
                                    .choose(&mut self.rng)
                            });
                            let Some(worktree) = worktree else { continue };
                            let full_path = worktree.read_with(cx, |worktree, _| {
                                let entry = worktree
                                    .entries(false)
                                    .filter(|e| e.is_file())
                                    .choose(&mut self.rng)
                                    .unwrap();
                                if entry.path.as_ref() == Path::new("") {
                                    Path::new(worktree.root_name()).into()
                                } else {
                                    Path::new(worktree.root_name()).join(&entry.path)
                                }
                            });
                            break ClientOperation::OpenBuffer {
                                project_root_name,
                                is_local,
                                full_path,
                            };
                        }
                    }
                }

                // Update a git related action
                91..=95 => {
                    break ClientOperation::GitOperation {
                        operation: self.generate_git_operation(client),
                    };
                }

                // Create or update a file or directory
                96.. => {
                    let is_dir = self.rng.gen::<bool>();
                    let content;
                    let mut path;
                    let dir_paths = client.fs.directories();

                    if is_dir {
                        content = String::new();
                        path = dir_paths.choose(&mut self.rng).unwrap().clone();
                        path.push(gen_file_name(&mut self.rng));
                    } else {
                        content = Alphanumeric.sample_string(&mut self.rng, 16);

                        // Create a new file or overwrite an existing file
                        let file_paths = client.fs.files();
                        if file_paths.is_empty() || self.rng.gen_bool(0.5) {
                            path = dir_paths.choose(&mut self.rng).unwrap().clone();
                            path.push(gen_file_name(&mut self.rng));
                            path.set_extension("rs");
                        } else {
                            path = file_paths.choose(&mut self.rng).unwrap().clone()
                        };
                    }
                    break ClientOperation::WriteFsEntry {
                        path,
                        is_dir,
                        content,
                    };
                }
            }
        })
    }

    fn generate_git_operation(&mut self, client: &TestClient) -> GitOperation {
        fn generate_file_paths(
            repo_path: &Path,
            rng: &mut StdRng,
            client: &TestClient,
        ) -> Vec<PathBuf> {
            let mut paths = client
                .fs
                .files()
                .into_iter()
                .filter(|path| path.starts_with(repo_path))
                .collect::<Vec<_>>();

            let count = rng.gen_range(0..=paths.len());
            paths.shuffle(rng);
            paths.truncate(count);

            paths
                .iter()
                .map(|path| path.strip_prefix(repo_path).unwrap().to_path_buf())
                .collect::<Vec<_>>()
        }

        let repo_path = client
            .fs
            .directories()
            .choose(&mut self.rng)
            .unwrap()
            .clone();

        match self.rng.gen_range(0..100_u32) {
            0..=25 => {
                let file_paths = generate_file_paths(&repo_path, &mut self.rng, client);

                let contents = file_paths
                    .into_iter()
                    .map(|path| (path, Alphanumeric.sample_string(&mut self.rng, 16)))
                    .collect();

                GitOperation::WriteGitIndex {
                    repo_path,
                    contents,
                }
            }
            26..=63 => {
                let new_branch = (self.rng.gen_range(0..10) > 3)
                    .then(|| Alphanumeric.sample_string(&mut self.rng, 8));

                GitOperation::WriteGitBranch {
                    repo_path,
                    new_branch,
                }
            }
            64..=100 => {
                let file_paths = generate_file_paths(&repo_path, &mut self.rng, client);

                let statuses = file_paths
                    .into_iter()
                    .map(|paths| {
                        (
                            paths,
                            match self.rng.gen_range(0..3_u32) {
                                0 => GitFileStatus::Added,
                                1 => GitFileStatus::Modified,
                                2 => GitFileStatus::Conflict,
                                _ => unreachable!(),
                            },
                        )
                    })
                    .collect::<Vec<_>>();

                GitOperation::WriteGitStatuses {
                    repo_path,
                    statuses,
                }
            }
            _ => unreachable!(),
        }
    }

    fn next_root_dir_name(&mut self, user_id: UserId) -> String {
        let user_ix = self
            .users
            .iter()
            .position(|user| user.user_id == user_id)
            .unwrap();
        let root_id = util::post_inc(&mut self.users[user_ix].next_root_id);
        format!("dir-{user_id}-{root_id}")
    }

    fn user(&mut self, user_id: UserId) -> &mut UserTestPlan {
        let ix = self
            .users
            .iter()
            .position(|user| user.user_id == user_id)
            .unwrap();
        &mut self.users[ix]
    }
}

async fn simulate_client(
    client: Rc<TestClient>,
    mut operation_rx: futures::channel::mpsc::UnboundedReceiver<usize>,
    plan: Arc<Mutex<TestPlan>>,
    mut cx: TestAppContext,
) {
    // Setup language server
    let mut language = Language::new(
        LanguageConfig {
            name: "Rust".into(),
            path_suffixes: vec!["rs".to_string()],
            ..Default::default()
        },
        None,
    );
    let _fake_language_servers = language
        .set_fake_lsp_adapter(Arc::new(FakeLspAdapter {
            name: "the-fake-language-server",
            capabilities: lsp::LanguageServer::full_capabilities(),
            initializer: Some(Box::new({
                let fs = client.fs.clone();
                move |fake_server: &mut FakeLanguageServer| {
                    fake_server.handle_request::<lsp::request::Completion, _, _>(
                        |_, _| async move {
                            Ok(Some(lsp::CompletionResponse::Array(vec![
                                lsp::CompletionItem {
                                    text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                                        range: lsp::Range::new(
                                            lsp::Position::new(0, 0),
                                            lsp::Position::new(0, 0),
                                        ),
                                        new_text: "the-new-text".to_string(),
                                    })),
                                    ..Default::default()
                                },
                            ])))
                        },
                    );

                    fake_server.handle_request::<lsp::request::CodeActionRequest, _, _>(
                        |_, _| async move {
                            Ok(Some(vec![lsp::CodeActionOrCommand::CodeAction(
                                lsp::CodeAction {
                                    title: "the-code-action".to_string(),
                                    ..Default::default()
                                },
                            )]))
                        },
                    );

                    fake_server.handle_request::<lsp::request::PrepareRenameRequest, _, _>(
                        |params, _| async move {
                            Ok(Some(lsp::PrepareRenameResponse::Range(lsp::Range::new(
                                params.position,
                                params.position,
                            ))))
                        },
                    );

                    fake_server.handle_request::<lsp::request::GotoDefinition, _, _>({
                        let fs = fs.clone();
                        move |_, cx| {
                            let background = cx.background();
                            let mut rng = background.rng();
                            let count = rng.gen_range::<usize, _>(1..3);
                            let files = fs.files();
                            let files = (0..count)
                                .map(|_| files.choose(&mut *rng).unwrap().clone())
                                .collect::<Vec<_>>();
                            async move {
                                log::info!("LSP: Returning definitions in files {:?}", &files);
                                Ok(Some(lsp::GotoDefinitionResponse::Array(
                                    files
                                        .into_iter()
                                        .map(|file| lsp::Location {
                                            uri: lsp::Url::from_file_path(file).unwrap(),
                                            range: Default::default(),
                                        })
                                        .collect(),
                                )))
                            }
                        }
                    });

                    fake_server.handle_request::<lsp::request::DocumentHighlightRequest, _, _>(
                        move |_, cx| {
                            let mut highlights = Vec::new();
                            let background = cx.background();
                            let mut rng = background.rng();

                            let highlight_count = rng.gen_range(1..=5);
                            for _ in 0..highlight_count {
                                let start_row = rng.gen_range(0..100);
                                let start_column = rng.gen_range(0..100);
                                let end_row = rng.gen_range(0..100);
                                let end_column = rng.gen_range(0..100);
                                let start = PointUtf16::new(start_row, start_column);
                                let end = PointUtf16::new(end_row, end_column);
                                let range = if start > end { end..start } else { start..end };
                                highlights.push(lsp::DocumentHighlight {
                                    range: range_to_lsp(range.clone()),
                                    kind: Some(lsp::DocumentHighlightKind::READ),
                                });
                            }
                            highlights.sort_unstable_by_key(|highlight| {
                                (highlight.range.start, highlight.range.end)
                            });
                            async move { Ok(Some(highlights)) }
                        },
                    );
                }
            })),
            ..Default::default()
        }))
        .await;
    client.language_registry.add(Arc::new(language));

    while let Some(batch_id) = operation_rx.next().await {
        let Some((operation, applied)) = plan.lock().next_client_operation(&client, batch_id, &cx) else { break };
        applied.store(true, SeqCst);
        match apply_client_operation(&client, operation, &mut cx).await {
            Ok(()) => {}
            Err(TestError::Inapplicable) => {
                applied.store(false, SeqCst);
                log::info!("skipped operation");
            }
            Err(TestError::Other(error)) => {
                log::error!("{} error: {}", client.username, error);
            }
        }
        cx.background().simulate_random_delay().await;
    }
    log::info!("{}: done", client.username);
}

fn buffer_for_full_path(
    client: &TestClient,
    project: &ModelHandle<Project>,
    full_path: &PathBuf,
    cx: &TestAppContext,
) -> Option<ModelHandle<language::Buffer>> {
    client
        .buffers_for_project(project)
        .iter()
        .find(|buffer| {
            buffer.read_with(cx, |buffer, cx| {
                buffer.file().unwrap().full_path(cx) == *full_path
            })
        })
        .cloned()
}

fn project_for_root_name(
    client: &TestClient,
    root_name: &str,
    cx: &TestAppContext,
) -> Option<ModelHandle<Project>> {
    if let Some(ix) = project_ix_for_root_name(&*client.local_projects(), root_name, cx) {
        return Some(client.local_projects()[ix].clone());
    }
    if let Some(ix) = project_ix_for_root_name(&*client.remote_projects(), root_name, cx) {
        return Some(client.remote_projects()[ix].clone());
    }
    None
}

fn project_ix_for_root_name(
    projects: &[ModelHandle<Project>],
    root_name: &str,
    cx: &TestAppContext,
) -> Option<usize> {
    projects.iter().position(|project| {
        project.read_with(cx, |project, cx| {
            let worktree = project.visible_worktrees(cx).next().unwrap();
            worktree.read(cx).root_name() == root_name
        })
    })
}

fn root_name_for_project(project: &ModelHandle<Project>, cx: &TestAppContext) -> String {
    project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .unwrap()
            .read(cx)
            .root_name()
            .to_string()
    })
}

fn project_path_for_full_path(
    project: &ModelHandle<Project>,
    full_path: &Path,
    cx: &TestAppContext,
) -> Option<ProjectPath> {
    let mut components = full_path.components();
    let root_name = components.next().unwrap().as_os_str().to_str().unwrap();
    let path = components.as_path().into();
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).find_map(|worktree| {
            let worktree = worktree.read(cx);
            if worktree.root_name() == root_name {
                Some(worktree.id())
            } else {
                None
            }
        })
    })?;
    Some(ProjectPath { worktree_id, path })
}

async fn ensure_project_shared(
    project: &ModelHandle<Project>,
    client: &TestClient,
    cx: &mut TestAppContext,
) {
    let first_root_name = root_name_for_project(project, cx);
    let active_call = cx.read(ActiveCall::global);
    if active_call.read_with(cx, |call, _| call.room().is_some())
        && project.read_with(cx, |project, _| project.is_local() && !project.is_shared())
    {
        match active_call
            .update(cx, |call, cx| call.share_project(project.clone(), cx))
            .await
        {
            Ok(project_id) => {
                log::info!(
                    "{}: shared project {} with id {}",
                    client.username,
                    first_root_name,
                    project_id
                );
            }
            Err(error) => {
                log::error!(
                    "{}: error sharing project {}: {:?}",
                    client.username,
                    first_root_name,
                    error
                );
            }
        }
    }
}

fn choose_random_project(client: &TestClient, rng: &mut StdRng) -> Option<ModelHandle<Project>> {
    client
        .local_projects()
        .iter()
        .chain(client.remote_projects().iter())
        .choose(rng)
        .cloned()
}

fn gen_file_name(rng: &mut StdRng) -> String {
    let mut name = String::new();
    for _ in 0..10 {
        let letter = rng.gen_range('a'..='z');
        name.push(letter);
    }
    name
}

fn path_env_var(name: &str) -> Option<PathBuf> {
    let value = env::var(name).ok()?;
    let mut path = PathBuf::from(value);
    if path.is_relative() {
        let mut abs_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        abs_path.pop();
        abs_path.pop();
        abs_path.push(path);
        path = abs_path
    }
    Some(path)
}
