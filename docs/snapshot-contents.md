# Contract: snapshot contents

A remote job runs against a snapshot of the caller's checkout, not against the
caller's files. `ck-motor` captures the snapshot on the caller's machine as a
Git commit, sends the history the runner lacks as Git bundles, and the runner
checks the commit out at the caller's path. This contract defines what the
snapshot contains. The code is `crates/motor-module/src/snapshot.rs` on the
client and `crates/motor-runner/src/workspace.rs` on the runner.

Rules carry the test that proves them. Tests in
`crates/motor-module/tests/it/` run in the portable suite; "operator-only" marks
a test that runs only in the [server gate](server-gate.md).

## 1. Which checkout

1.1. `workspace_key`, `repository_root` and `cwd` must be absolute. The key must
be the top of a Git worktree, `repository_root` the top of its repository, and
`cwd` inside the key's own checkout, not in a nested repository or another
worktree. A violation refuses the job before capture with
`workspace_key_rejected`. Tests: `cwd_in_nested_checkouts_is_refused_before_snapshot`,
`subdirectories_cannot_pose_as_main_or_linked_roots`,
`main_and_linked_workspaces_are_admitted_by_repository_identity`
(`workspace_capture.rs`).

1.2. Each `--sibling` / `siblings` entry is another repository root captured by
the same rules and sent with the job. A sibling inside the workspace, or a
workspace nested inside a known one, is refused before setup. Tests:
`sibling_inside_a_known_workspace_is_refused_before_setup`,
`nested_workspace_is_refused_before_any_workspace_is_created`
(`crates/motor-runner/src/tests/nested_workspaces.rs`).

## 2. What goes in

The optional cache-fill tip is separate from the submitted snapshot commit.
`ck-motor` names it only after checking the local default-branch candidate's
name and commit against the configured origin's default branch, with bounded
successful-result caching. Local refs alone cannot authorize a fill. A failed
check leaves the fill tip empty and does not change or refuse the snapshot.
See [compile-cache.md](compile-cache.md#verifying-the-fill-tip). Tests:
`local_tip_ahead_of_remote_has_no_tip`, `unreachable_remote_has_no_tip`,
`current_client_unverified_default_tip_runs_and_prepares_without_fills`.

The snapshot's tree starts from `HEAD` and then takes the working tree's
current state:

2.1. Every tracked file, with its working-tree content, including uncommitted
and unstaged changes and deletions. The caller's staged index is not used as
the base; it only contributes rule 2.3.

2.2. Every untracked file that no ignore rule excludes, as Git's standard
exclude rules decide (`.gitignore` files, `.git/info/exclude` and the user's
`core.excludesFile`).

2.3. A file the caller's real index tracks, even if it matches an ignore rule,
as long as it still exists in the working tree. Tests:
`staged_file_under_an_ignored_path_reaches_the_job`,
`staged_ignored_literal_names_and_deleted_paths_reach_the_job` (`safety.rs`),
`tree_hash_includes_force_added_ignored_inputs_from_the_real_index`
(`crates/motor-runner/src/tests/workspace_snapshots.rs`).

Tracked files inside ignored directories remain source inputs; ignored,
untracked siblings stay out. Capture uses recursive `git add -A -- .`, not an
explicit list of positive pathspecs, because Git can reject an explicitly named
ignored directory even when it contains tracked files. Test:
`tracked_file_under_ignored_directory_is_captured_without_ignored_sibling`
(`snapshot.rs`).

2.4. A tracked submodule stays a gitlink at its recorded commit. Test:
`tracked_submodule_gitlink_is_preserved_in_snapshot` (`workspace_capture.rs`).

2.5. File modes and symlinks are recorded as Git records them: executable bit
and symlink target, not the target's contents. Not covered by a dedicated test
on the Linux path; on Windows see [windows-jobs.md](windows-jobs.md#4-paths).

## 3. What stays out

3.1. Untracked files that an ignore rule excludes never leave the caller's
machine. Test: `failed_bundle_deletes_temporary_refs_and_ignored_files_never_transfer`
(`safety.rs`).

3.2. Exclude the root `target` entry and all its descendants regardless of Git
tracking or ignore status; nested directories such as `src/target` are source.
The runner owns the top-level directory as the job's build output. Both capture
and runner verification remove entries inherited from `HEAD`, not just staged
and untracked files. Fingerprinting and the untracked size cap use the same
exclusion, for the workspace and every sibling. Committed target files can
still travel in Git history, but are absent from the snapshot tree and are not
restored as source inputs. Tests:
`workspace_and_sibling_exclude_all_top_level_target_but_keep_nested_target`,
`staged_file_under_target_is_not_sent_and_does_not_block_the_job` (`safety.rs`),
`untracked_top_level_target_does_not_change_snapshot_or_fingerprint`,
`top_level_target_file_symlink_and_repository_are_excluded`,
`nested_target_is_source_but_committed_top_level_target_is_not`,
`untracked_size_cap_excludes_only_top_level_target` (`snapshot.rs`),
`staged_target_paths_never_enter_the_tree_hash`,
`committed_top_level_target_is_excluded_but_nested_target_is_captured`
(`crates/motor-runner/src/tests/workspace_snapshots.rs`).

3.3. An untracked nested repository or linked worktree inside the checkout is
left out, along with its contents. Tests:
`untracked_linked_worktree_and_nested_repo_are_excluded_from_snapshot_tree`,
`head_workspace_job_with_untracked_worktree_and_nested_repo_runs`
(`workspace_capture.rs`).

3.4. Nothing from `.git` is sent except the commits and objects the runner
lacks. Local Git configuration, hooks and the caller's index are not sent.
Not covered by a test.

## 4. The size cap

4.1. The untracked files admitted by rule 2.2, summed over the workspace and
all its siblings, may total at most 2 GiB (`UNTRACKED_CONTENT_LIMIT`,
2147483648 bytes). Tracked files do not count against the cap.

4.2. The check reads only file metadata: regular files and symlinks count at
their own size; a symlink's target and ignored files do not count. Test:
`untracked_size_preflight_excludes_ignored_files_and_symlink_targets`.

4.3. Over the cap, the job is refused before capture or upload with
`snapshot_failed` and `ran: none`. The refusal detail names the total, the
limit and the three largest files. Nothing is sent to the runner. `prepare`
refuses the same way. Test: `oversized_untracked_content_is_refused_before_capture_or_upload`.

4.4. A total of exactly 2147483648 bytes is admitted (the comparison is
strictly greater than). The boundary itself is not covered by a test; the test
uses one byte over.

## 5. Consistency

5.1. The capture must see a quiet checkout. `ck-motor` fingerprints the
checkout before and after capture; if it changed, it tries again, up to three
attempts, then refuses with `snapshot_failed`. Tests:
`changing_checkout_is_refused_after_three_capture_attempts`,
`one_checkout_change_retries_then_captures_the_quiet_state`,
`capture_fingerprint_detects_transient_untracked_files_and_head_changes`,
`fingerprint_includes_ignored_ignore_and_attribute_files`.

A failed Git command includes its first stderr line in the snapshot refusal
and log. Request and process environment values of at least eight bytes and
snapshot state paths are redacted before selecting the line and applying the
1024-byte limit. Tests:
`failed_git_add_reports_first_stderr_line_with_request_secrets_redacted`,
`snapshots_preserve_head_real_index_status_and_refs_on_success_and_failure`
(`safety.rs`), `snapshot_refusal_keeps_first_error_line_and_redacts_before_bounding`
(`motor.rs`).

5.2. Capture never changes the caller's checkout: `HEAD`, the branch, the real
index, working files and refs stay as they were, on success and on failure.
Temporary refs under `refs/ck-motor/` are deleted, and leftovers older than an
hour are pruned. Tests:
`snapshots_preserve_head_real_index_status_and_refs_on_success_and_failure`
(`safety.rs`), `head_snapshot_leaves_head_branch_real_index_and_working_files_untouched`
(`workspace_capture.rs`).

5.3. The runner recomputes the tree hash of what it checked out and refuses
with `tree_hash_mismatch` when it differs from the hash `ck-motor` sent; the
command does not start. The logged refusal and `refusal_detail` include the
computed tree and up to five differing paths from `git diff-tree`, plus the
number of remaining paths. If the caller's tree cannot be read, the detail
says that comparison is unavailable. Details retain the usual redaction and
1024-byte bound. Tests: `hash_mismatch_and_materialization_failure_never_start`
(`crates/motor-runner/src/tests.rs`),
`mismatch_refusal_names_computed_tree_and_bounded_differing_paths`
(`crates/motor-runner/src/tests/workspace_snapshots.rs`).

## 6. Where it lands

6.1. On Linux the runner checks the snapshot out at the same absolute path as
the caller's checkout, so absolute paths in build output, error messages and
`cwd` match the caller's. The destination must lie under an owned workspace
root (default `/Users`; add more with `serve --owned-workspace-root`). Paths
outside, including through a symlink, are refused before anything is written.
Tests: `destination_roots_refuse_run_prepare_and_drop_before_mutation`,
`destination_roots_check_future_paths_symlink_cwd_and_heads`
(`crates/motor-runner/src/tests/workspace_lifetime.rs`).

6.2. On Windows the same path is mapped under `C:\m`; see
[windows-jobs.md](windows-jobs.md#4-paths).

6.3. Changes a job makes to the workspace stay on the runner. They are not
copied back; the terminal record lists changed paths in `workspace_changes`.
Not covered by a test in this repository.

## 7. History transfer

7.1. Before uploading, `ck-motor` and the runner exchange which commits each
side has (`have` / `have_reply`), and only missing history is bundled. Test:
`unchanged_random_file_is_excluded_from_second_bundle` (`safety.rs`).

7.2. Git dependencies pinned in `Cargo.lock` are sent as separate bundles when
the runner advertises support; older runners receive none. Tests:
`run_and_prepare_lockfile_source_is_commit_not_worktree`,
`old_runner_without_capability_receives_no_dependency_chunks`
(`git_dependencies.rs`).
