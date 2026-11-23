# Sharetree in a T3 project

For a single Git repository, use the creation command from [T3's custom worktree layout](https://github.com/pingdotgg/t3code/blob/v0.1.12-nightly.20261122.5111/docs/user/multi-repo-projects.md#lay-the-folder-out-yourself). Copy the script into the project and merge this entry into its `t3.json`.

```json
{
  "worktreeLayout": {
    "create": "sh ./integrations/t3/create-worktree.sh"
  }
}
```

Install `sharetree` on the T3 server's PATH. Put the project root and T3 worktree directory on a filesystem that supports sharing blocks. The script uses the original project root as the donor and creates or restores the branch requested by T3. If Sharetree is absent, the script uses Git. If Sharetree starts, its exit status goes to T3 so checkout and hook failures take T3's rollback path.

Use a disposable T3 project first. Verify branch and commit parity, a clean checkout, a positive reflink count, and removal through T3. Then repeat with an existing branch to verify restoration. The script is for POSIX shells and single-repository layouts.
