import argparse
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from hashlib import sha256
from pathlib import Path, PurePosixPath

import tree_sitter_bash
import tree_sitter_powershell
import tree_sitter_python
import tree_sitter_rust
import tree_sitter_toml
import tree_sitter_yaml
import yaml
from tree_sitter import Language, Parser

ROOT = Path(__file__).resolve().parent.parent
BASELINE = Path("scripts/comment-baseline.txt")
PARSERS = {
    "bash": Parser(Language(tree_sitter_bash.language())),
    "pwsh": Parser(Language(tree_sitter_powershell.language())),
    "python": Parser(Language(tree_sitter_python.language())),
    "rust": Parser(Language(tree_sitter_rust.language())),
    "toml": Parser(Language(tree_sitter_toml.language())),
    "yaml": Parser(Language(tree_sitter_yaml.language())),
}
SUFFIXES = {
    ".rs": "rust",
    ".sh": "bash",
    ".bash": "bash",
    ".ps1": "pwsh",
    ".py": "python",
    ".toml": "toml",
    ".yaml": "yaml",
    ".yml": "yaml",
}
COMMENT_TYPES = {"comment", "line_comment", "block_comment"}
WORKFLOW_EXPRESSION = re.compile(rb"\$\{\{.*?\}\}", re.DOTALL)
MATRIX_OS_EXPRESSION = re.compile(r"\$\{\{\s*matrix\.os\s*\}\}")
POWERSHELL_REQUIRES = re.compile(rb"#requires[ \t]+-([a-z]+)(?=[ \t\r:]|$)", re.IGNORECASE)
POWERSHELL_REQUIREMENTS = (
    b"assembly",
    b"modules",
    b"psedition",
    b"pssnapin",
    b"runasadministrator",
    b"shellid",
    b"version",
)
POWERSHELL_EXPANDABLE_STRINGS = {"expandable_here_string_literal", "expandable_string_literal"}


@dataclass(frozen=True)
class Comment:
    path: str
    line: int
    column: int
    contents: bytes
    full_lines: frozenset[int]

    @property
    def key(self):
        return f"{sha256(self.contents).hexdigest()}\t{self.path}"


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)


def language(path):
    name = PurePosixPath(path).name
    if name == "PKGBUILD":
        return "bash"
    if name == ".gitignore":
        return "git-config"
    if name == "comment-lint-requirements.txt":
        return "requirements"
    return SUFFIXES.get(PurePosixPath(path).suffix)


def is_powershell_comment(node):
    parent = node.parent
    if parent.type == "expandable_here_string_literal":
        return False
    directive = POWERSHELL_REQUIRES.match(node.text)
    if not directive or not any(
        parameter.startswith(directive[1].lower()) for parameter in POWERSHELL_REQUIREMENTS
    ):
        return True
    while parent is not None:
        if parent.type in POWERSHELL_EXPANDABLE_STRINGS:
            return True
        parent = parent.parent
    return False


def syntax_comments(source, kind):
    remaining = [PARSERS[kind].parse(source).root_node]
    while remaining:
        node = remaining.pop()
        if node.type in COMMENT_TYPES:
            if kind == "pwsh" and not is_powershell_comment(node):
                continue
            yield node.start_point, node.end_point, node.start_byte, node.end_byte
        else:
            remaining.extend(reversed(node.children))


def text_comments(source, kind):
    offset = 0
    for row, raw_line in enumerate(source.splitlines(keepends=True)):
        line = raw_line.rstrip(b"\r\n")
        if kind == "requirements":
            match = re.search(rb"(?:^|\s)#", line)
            column = match.end() - 1 if match else None
        else:
            column = 0 if line.startswith(b"#") else None
        if column is not None:
            yield (row, column), (row, len(line)), offset + column, offset + len(line)
        offset += len(raw_line)


def source_comments(path, source, kind, *, line_offset=0, shebang=False, masked=None):
    syntax = source if masked is None else masked
    lines = source.split(b"\n")
    if kind in PARSERS:
        spans = syntax_comments(syntax, kind)
    else:
        spans = text_comments(syntax, kind)
    for (start_row, start_col), (end_row, end_col), start, end in spans:
        if shebang and start == 0 and source.startswith(b"#!"):
            continue
        if end_col == 0 and end_row > start_row:
            end_row -= 1
            end_col = len(lines[end_row])
        first_full_line = start_row + bool(lines[start_row][:start_col].strip())
        last_full_line = end_row - bool(lines[end_row][end_col:].strip())
        full_lines = range(first_full_line + line_offset + 1, last_full_line + line_offset + 2)
        yield Comment(
            path,
            start_row + line_offset + 1,
            start_col,
            source[start:end].rstrip(b"\n"),
            frozenset(full_lines),
        )


def fields(node):
    if isinstance(node, yaml.MappingNode):
        return {key.value: value for key, value in node.value if isinstance(key, yaml.ScalarNode)}
    return {}


def default_shell(mapping):
    return fields(fields(mapping.get("defaults")).get("run")).get("shell")


def runner_shells(job):
    runner = job.get("runs-on")
    labels = [runner]
    if isinstance(runner, yaml.ScalarNode) and MATRIX_OS_EXPRESSION.fullmatch(runner.value):
        matrix = fields(fields(job.get("strategy")).get("matrix"))
        axis = matrix.get("os")
        include = matrix.get("include")
        labels = list(axis.value) if isinstance(axis, yaml.SequenceNode) else []
        if isinstance(include, yaml.SequenceNode):
            labels.extend(fields(row).get("os") for row in include.value)
    shells = {
        "pwsh" if label.value.lower().startswith("windows-") else "bash"
        for label in labels
        if isinstance(label, yaml.ScalarNode)
    }
    return sorted(shells) or ["bash"]


def workflow_scripts(path, source):
    workflow = fields(yaml.compose(source))
    workflow_shell = default_shell(workflow)
    for job in fields(workflow.get("jobs")).values():
        job_fields = fields(job)
        steps = job_fields.get("steps")
        if not isinstance(steps, yaml.SequenceNode):
            continue
        implicit_shells = runner_shells(job_fields)
        for step in steps.value:
            step_fields = fields(step)
            run = step_fields.get("run")
            if not isinstance(run, yaml.ScalarNode):
                continue
            shell = step_fields.get("shell") or default_shell(job_fields) or workflow_shell
            kinds = implicit_shells
            if shell:
                kind = shell.value.split()[0]
                kind = {"sh": "bash", "powershell": "pwsh"}.get(kind, kind)
                if kind not in {"bash", "pwsh", "python"}:
                    raise ValueError(
                        f"{path}:{run.start_mark.line + 1}: comment lint does not support shell {kind!r}"
                    )
                kinds = [kind]
            data = run.value.encode()
            masked = WORKFLOW_EXPRESSION.sub(lambda match: re.sub(rb"[^\n]", b"x", match[0]), data)
            line_offset = run.start_mark.line + (run.style in {"|", ">"})
            found = set()
            for kind in kinds:
                found.update(source_comments(path, data, kind, line_offset=line_offset, masked=masked))
            yield from sorted(found, key=lambda comment: (comment.line, comment.column, comment.contents))


def comments(ref=None):
    if ref:
        files = git("ls-tree", "-rz", "--name-only", ref)
    else:
        files = git("ls-files", "--cached", "--others", "--exclude-standard", "-z")
    for path in sorted(set(files.decode().split("\0"))):
        kind = language(path)
        if kind is None:
            continue
        if ref:
            source = git("show", f"{ref}:{path}")
        else:
            file = ROOT / path
            if not file.is_file() or file.is_symlink():
                continue
            source = file.read_bytes()
        source = source.replace(b"\r\n", b"\n")
        yield from source_comments(path, source, kind, shebang=kind in {"bash", "python"})
        parts = PurePosixPath(path).parts
        if kind == "yaml" and parts[:2] == (".github", "workflows"):
            yield from workflow_scripts(path, source)


def baseline_from(contents):
    return Counter(contents.decode().splitlines())


def main():
    parser = argparse.ArgumentParser(
        description="Block new source comments and ratchet the checked-in baseline."
    )
    parser.add_argument("--base", help="Git revision whose baseline bounds the current baseline")
    output = parser.add_mutually_exclusive_group()
    output.add_argument(
        "--list",
        action="store_true",
        help="list recognized comments and count their physical lines",
    )
    output.add_argument(
        "--write-baseline",
        action="store_true",
        help="sync the baseline after deleting comments",
    )
    parser.add_argument("--ref", help="inspect a historical Git revision with --list")
    args = parser.parse_args()
    if args.ref and not args.list:
        parser.error("--ref requires --list")
    found = list(comments(args.ref))
    if args.list:
        for comment in found:
            print(
                f"{comment.path}:{comment.line}: {comment.contents.decode(errors='backslashreplace')}"
            )
        full_lines = {(comment.path, line) for comment in found for line in comment.full_lines}
        occupied_lines = {
            (comment.path, comment.line + offset)
            for comment in found
            for offset in range(len(comment.contents.splitlines()))
        }
        print(
            f"{len(found)} comment tokens on {len(occupied_lines)} lines, {len(full_lines)} full lines"
        )
        return 0
    actual = Counter(comment.key for comment in found)
    baseline = ROOT / BASELINE
    if args.base:
        git("rev-parse", "--verify", f"{args.base}^{{commit}}")
        prior = git("ls-tree", "--name-only", args.base, str(BASELINE))
        allowed = (
            baseline_from(git("show", f"{args.base}:{BASELINE}"))
            if prior
            else Counter(comment.key for comment in comments(args.base))
        )
    else:
        allowed = baseline_from(baseline.read_bytes())
    expected = actual if args.write_baseline else baseline_from(baseline.read_bytes())
    extra = (actual - allowed) | (expected - allowed) | (actual - expected)
    for comment in found:
        if extra[comment.key]:
            first_line = comment.contents.splitlines()[0].decode(errors="backslashreplace")
            print(
                f"{comment.path}:{comment.line}: new comment: {first_line}",
                file=sys.stderr,
            )
    if extra:
        print("Comment additions and baseline growth are prohibited.", file=sys.stderr)
        return 1
    if args.write_baseline:
        baseline.write_text("".join(f"{entry}\n" for entry in sorted(actual.elements())))
    elif expected - actual:
        print(
            "Comment baseline has stale entries. Run the lint with --write-baseline.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
