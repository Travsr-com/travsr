import * as vscode from "vscode";

// The built-in Git extension's API is not in @types/vscode, so we declare the
// minimal structural shape we read: the list of open repositories and their
// root folders. `getExtension<T>().exports` is trusted to match at runtime.
interface GitRepository {
  rootUri: vscode.Uri;
}
interface GitApi {
  repositories: GitRepository[];
  onDidOpenRepository: vscode.Event<GitRepository>;
  onDidCloseRepository: vscode.Event<GitRepository>;
}
interface GitExtensionExports {
  getAPI(version: 1): GitApi;
}

const STATE_KEY = "travsr.activeRepo";

/** The built-in Git extension's API, or undefined when it is disabled or has not
 *  activated yet. */
function gitApi(): GitApi | undefined {
  try {
    return vscode.extensions
      .getExtension<GitExtensionExports>("vscode.git")
      ?.exports?.getAPI(1);
  } catch {
    return undefined;
  }
}

/**
 * Every git repo open in the window.
 *
 * The Git extension knows every repo under every workspace folder — including
 * several sibling repos inside a single opened folder (`~/projects/` holding
 * `repoA/`, `repoB/`), which a `workspaceFolders` scan alone would miss because
 * that folder is not itself a repo. Workspace folders are unioned in as a
 * fallback for when the Git extension is disabled or has not activated yet.
 */
function discoverRepos(): string[] {
  const paths = new Set<string>();
  for (const repo of gitApi()?.repositories ?? []) {
    paths.add(repo.rootUri.fsPath);
  }
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    paths.add(folder.uri.fsPath);
  }
  return [...paths];
}

function basename(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() ?? p;
}

/**
 * The single repository the extension targets for `lang install` / `lang detect`
 * / `init`. Chosen once by the user (persisted per-workspace) rather than guessed
 * per action, and surfaced in a clickable status-bar item so it is always clear
 * which repo a language install will land in.
 */
export class ActiveRepo {
  private readonly item: vscode.StatusBarItem;

  constructor(private readonly context: vscode.ExtensionContext) {
    this.item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 0);
    this.item.command = "travsr.selectRepository";
    this.item.tooltip =
      "Travsr targets this repository for language install / detect. Click to change.";
    context.subscriptions.push(this.item);
    this.refresh();
    context.subscriptions.push(
      vscode.workspace.onDidChangeWorkspaceFolders(() => this.refresh())
    );
    // Nested repos can surface a moment after the Git extension activates; keep
    // the indicator in sync as repos open and close.
    const git = gitApi();
    if (git) {
      context.subscriptions.push(
        git.onDidOpenRepository(() => this.refresh()),
        git.onDidCloseRepository(() => this.refresh())
      );
    }
  }

  /** True when more than one repo is open, so the choice actually matters. */
  hasChoice(): boolean {
    return discoverRepos().length > 1;
  }

  /**
   * Absolute path of the targeted repo, or `undefined` when no repo is open.
   * One repo → that repo (unambiguous). Several → the saved pick if still open,
   * else the first, so an action never silently no-ops before a pick is made.
   */
  current(): string | undefined {
    const repos = discoverRepos();
    if (repos.length === 0) return undefined;
    const saved = this.context.workspaceState.get<string>(STATE_KEY);
    if (saved && repos.includes(saved)) return saved;
    return repos[0];
  }

  /** Short name of the targeted repo for display, or `undefined` when none. */
  currentName(): string | undefined {
    const cur = this.current();
    return cur ? basename(cur) : undefined;
  }

  /** The user's explicit pick for this workspace, if one is still open. */
  private saved(): string | undefined {
    const s = this.context.workspaceState.get<string>(STATE_KEY);
    return s && discoverRepos().includes(s) ? s : undefined;
  }

  /**
   * Resolve the target for an action that is about to run in a repo. Prompts once
   * when the choice is genuinely ambiguous — several repos open and none picked
   * yet — then remembers it. Returns `undefined` if no repo is open or the user
   * dismissed the prompt, in which case the caller should abort.
   */
  async ensureChosen(): Promise<string | undefined> {
    if (this.hasChoice() && !this.saved()) {
      await this.pick();
      return this.saved();
    }
    return this.current();
  }

  /** Show the target in the status bar, but only when there is a choice to make;
   *  a single-repo window has no ambiguity and needs no extra chrome. */
  refresh(): void {
    if (this.hasChoice()) {
      this.item.text = `$(repo) Travsr: ${this.currentName()} $(chevron-down)`;
      this.item.show();
    } else {
      this.item.hide();
    }
  }

  /** Quick-pick to choose which repo the extension targets. */
  async pick(): Promise<void> {
    const repos = discoverRepos();
    if (repos.length <= 1) {
      void vscode.window.showInformationMessage(
        repos.length === 0
          ? "No git repository is open."
          : "Only one repository is open, nothing to choose."
      );
      return;
    }
    const cur = this.current();
    const picked = await vscode.window.showQuickPick(
      repos.map((p) => ({
        label: basename(p),
        description: p === cur ? "current target" : "",
        detail: p,
        repoPath: p,
      })),
      {
        title: "Travsr: repository to target for language install / detect",
        placeHolder: cur,
      }
    );
    if (!picked) return;
    await this.context.workspaceState.update(STATE_KEY, picked.repoPath);
    this.refresh();
  }
}
