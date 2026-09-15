"""Enforce production Rust file sizes without building or expanding macros."""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import NamedTuple

from tree_sitter import Language, Parser, Query, QueryCursor
import tree_sitter_rust


LIMIT = 500
POLICY = ".rust-file-sizes.json"
COMMENTS = {"line_comment", "block_comment"}
RUST = Language(tree_sitter_rust.language())
PARSER = Parser(RUST)
ATTRIBUTES = Query(RUST, "[(attribute_item) (inner_attribute_item)] @attribute")


class Counts(NamedTuple):
    production: int
    tests: int
    total: int


def cfg_value(tokens, test):
    """Evaluate only test; other configurations remain unknown, never disabled."""
    tokens = [node for node in tokens if node.type not in COMMENTS]
    if len(tokens) == 1 and tokens[0].type == "identifier":
        return test if tokens[0].text == b"test" else None
    if len(tokens) != 2 or tokens[1].type != "token_tree":
        return None
    name, arguments = tokens
    groups = [[]]
    for node in arguments.children[1:-1]:
        if node.type == ",":
            groups.append([])
        elif node.type not in COMMENTS:
            groups[-1].append(node)
    if not groups[-1]:
        groups.pop()  # Empty argument list or a trailing comma.
    values = [cfg_value(group, test) for group in groups]
    if name.text == b"all":
        return False if False in values else (True if all(v is True for v in values) else None)
    if name.text == b"any":
        return True if True in values else (False if all(v is False for v in values) else None)
    if name.text == b"not" and len(values) == 1 and values[0] is not None:
        return not values[0]
    return None


def test_attribute(node):
    attribute = next((child for child in node.named_children if child.type == "attribute"), None)
    if attribute is None:
        return False
    parts = [child for child in attribute.named_children if child.type not in COMMENTS]
    if len(parts) == 1 and parts[0].text in (b"test", b"bench"):
        return True
    if len(parts) != 2 or parts[0].text != b"cfg" or parts[1].type != "token_tree":
        return False
    tokens = parts[1].children[1:-1]
    return cfg_value(tokens, False) is False and cfg_value(tokens, True) is not False


def attributed_start(node):
    start = node.start_byte
    previous = node.prev_named_sibling
    while previous is not None and previous.type in COMMENTS | {"attribute_item"}:
        if previous.type == "attribute_item":
            start = previous.start_byte
        previous = previous.prev_named_sibling
    return start


def test_ranges(node):
    """Yield complete test-only syntax ranges, including their outer attributes."""
    for attribute in QueryCursor(ATTRIBUTES).captures(node).get("attribute", []):
        if not test_attribute(attribute):
            continue
        if attribute.type == "inner_attribute_item":
            owner = attribute.parent
            if owner.parent is not None and owner.parent.child_by_field_name("body") == owner:
                owner = owner.parent
            yield attributed_start(owner), owner.end_byte
            continue
        owner = attribute.parent
        if owner.type not in {"match_arm", "field_initializer", "shorthand_field_initializer"}:
            owner = attribute.next_named_sibling
            # Tuple fields store visibility and type as separate siblings.
            while owner is not None and owner.type in COMMENTS | {"attribute_item", "visibility_modifier"}:
                owner = owner.next_named_sibling
        if owner is None:
            raise ValueError("test attribute has no following Rust item")
        end = owner.end_byte
        if owner.next_sibling is not None and owner.next_sibling.type in {",", ";"}:
            end = owner.next_sibling.end_byte
        yield attributed_start(attribute), end


def count_source(source):
    source.decode("utf-8")  # Fail explicitly on malformed source encoding.
    tree = PARSER.parse(source)
    if tree.root_node.has_error:
        raise ValueError("cannot parse Rust; fix syntax or update the pinned Rust grammar")
    remaining = bytearray(source)
    test_rows = set()
    for start, end in test_ranges(tree.root_node):
        first = source.count(b"\n", 0, start)
        last = source.count(b"\n", 0, end - 1)
        test_rows.update(range(first, last + 1))
        remaining[start:end] = bytes(10 if byte == 10 else 32 for byte in source[start:end])
    lines = bytes(remaining).split(b"\n") if source else []
    if source.endswith(b"\n"):
        lines.pop()
    production = sum(row not in test_rows or bool(line.strip()) for row, line in enumerate(lines))
    return Counts(production, len(test_rows), len(lines))


def dedicated_test(path):
    parts = Path(path).parts
    return bool({"tests", "benches", "examples"}.intersection(parts)) or parts[-1] == "tests.rs"


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate policy key: {key}")
        result[key] = value
    return result


def load_policy(root, paths):
    policy = json.loads((root / POLICY).read_text(encoding="utf-8"), object_pairs_hook=unique_keys)
    if not isinstance(policy, dict) or set(policy) != {"version", "baseline", "exceptions", "generated"}:
        raise ValueError("policy must contain version, baseline, exceptions, and generated")
    if type(policy["version"]) is not int or policy["version"] != 1:
        raise ValueError("unsupported policy version")
    for section in ("baseline", "exceptions", "generated"):
        entries = policy[section]
        if not isinstance(entries, dict):
            raise ValueError(f"{section} must be an object")
        for path, entry in entries.items():
            if path not in paths or dedicated_test(path):
                raise ValueError(f"{path}: stale {section} entry; remove or update the path")
            if section == "baseline":
                if type(entry) is not int or entry <= LIMIT:
                    raise ValueError(f"{path}: baseline must be an integer above {LIMIT}")
            elif section == "exceptions":
                if not isinstance(entry, dict) or set(entry) != {"limit", "reason"}:
                    raise ValueError(f"{path}: exception requires limit and reason")
                if type(entry["limit"]) is not int or entry["limit"] <= policy["baseline"].get(path, LIMIT):
                    raise ValueError(f"{path}: exception limit must exceed its normal allowance")
                if not isinstance(entry["reason"], str) or not entry["reason"].strip():
                    raise ValueError(f"{path}: exception requires a documented reason")
            elif not isinstance(entry, str) or not entry.strip():
                raise ValueError(f"{path}: generated exclusion requires a generator/reason")
    overlap = set(policy["generated"]) & (set(policy["baseline"]) | set(policy["exceptions"]))
    if overlap:
        raise ValueError(f"generated exclusions also have limits: {', '.join(sorted(overlap))}")
    return policy


def check(root, report=False):
    listed = subprocess.check_output(["git", "ls-files", "-z", "--", "*.rs"], cwd=root)
    paths = sorted({os.fsdecode(path) for path in listed.split(b"\0") if path})
    policy = load_policy(root, paths)
    errors = []
    checked = 0
    for path in paths:
        if dedicated_test(path):
            continue
        file = root / path
        if file.is_symlink() or not file.is_file():
            errors.append(f"{path}: expected a regular file; remove stale policy entries when deleting files")
            continue
        if path in policy["generated"]:
            continue
        try:
            counts = count_source(file.read_bytes())
        except (OSError, ValueError) as error:
            errors.append(f"{path}: {error}")
            continue
        checked += 1
        baseline = policy["baseline"].get(path)
        exception = policy["exceptions"].get(path)
        allowed = exception["limit"] if exception else (baseline or LIMIT)
        if report:
            print(f"{path}: {counts.production} production, {counts.tests} test, {counts.total} total; limit {allowed}")
        if counts.production > allowed:
            errors.append(
                f"{path}: {counts.production} production lines, limit {allowed}; "
                "split by responsibility or request a reviewed exception with a reason"
            )
        if baseline is not None and counts.production < baseline:
            action = "remove its baseline entry" if counts.production <= LIMIT else f"lower its baseline to {counts.production}"
            errors.append(f"{path}: {counts.production} production lines, baseline {baseline}; {action}")
        if exception and counts.production <= (baseline or LIMIT):
            errors.append(f"{path}: {counts.production} production lines; remove the unused exception")
    for error in errors:
        print(error, file=sys.stderr)
    if not errors:
        print(f"Rust file sizes: {checked} production files checked (limit {LIMIT}; explicit baseline/exceptions).")
    return bool(errors)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--report", action="store_true", help="Print counts without modifying the policy")
    args = parser.parse_args()
    try:
        return int(check(args.root, args.report))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Rust file sizes: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
