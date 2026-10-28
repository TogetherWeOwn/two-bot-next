"""Preserve workspace-package features for isolated test selections.

External metadata features can merge platform/build contexts. Do not copy
those flags into a host test build; real Cargo/LLVM parity remains required.
"""


def workspace_feature_args(metadata, selected_names):
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    if not members or not members.issubset(packages) or not members.issubset(nodes):
        raise ValueError("Cargo metadata is missing workspace dependency resolution")
    selected = [package for key, package in packages.items()
                if key in members and package["name"] in selected_names]
    if {package["name"] for package in selected} != set(selected_names):
        raise ValueError("feature selection names an unknown workspace package")
    core = next((key for key in members if packages[key]["name"] == "two-bot-core"), None)
    if core is None:
        raise ValueError("Cargo metadata has no core package")

    def dependencies(key):
        if key not in nodes:
            raise ValueError("Cargo metadata has an incomplete dependency graph")
        return nodes[key]["deps"]

    def reaches_core(key):
        pending, seen = [key], set()
        while pending:
            current = pending.pop()
            if current in seen:
                continue
            seen.add(current)
            if current == core:
                return True
            pending.extend(dep["pkg"] for dep in dependencies(current))
        return False

    def forwards_core(key, feature, seen=None):
        if key == core and feature == "db":
            return True
        seen = set() if seen is None else seen
        if (key, feature) in seen:
            return False
        seen.add((key, feature))
        for expansion in packages[key].get("features", {}).get(feature, []):
            if expansion.startswith("dep:"):
                continue
            if "/" not in expansion:
                if forwards_core(key, expansion, seen):
                    return True
                continue
            alias, target_feature = expansion.split("/", 1)
            alias = alias.removesuffix("?").replace("-", "_")
            for dep in dependencies(key):
                if dep["name"].replace("-", "_") == alias:
                    if forwards_core(dep["pkg"], target_feature, seen):
                        return True
        return False

    flags = []
    for package in sorted(selected, key=lambda package: package["name"]):
        key = package["id"]
        own = sorted(nodes[key].get("features", []))
        if any(feature not in package.get("features", {}) for feature in own):
            raise ValueError("resolved package feature has no declaration")
        flags.extend(package["name"] + "/" + feature for feature in own)
        reproduced_core = any(forwards_core(key, feature) for feature in own)
        # Direct dependency flags also preserve features unified by a different
        # workspace root, not just features the consumer itself forwards.
        for dep in package["dependencies"]:
            alias = dep.get("rename") or dep["name"]
            resolved = [edge for edge in dependencies(key)
                        if edge["name"].replace("-", "_") == alias.replace("-", "_")]
            for edge in resolved:
                target = edge["pkg"]
                if target not in nodes or target not in packages:
                    raise ValueError("Cargo metadata has an unresolved dependency")
                if target not in members:
                    continue
                enabled = sorted(nodes[target].get("features", []))
                flags.extend(alias + "/" + feature for feature in enabled)
                reproduced_core |= any(forwards_core(target, feature) for feature in enabled)
        if ("db" in nodes[core].get("features", [])
                and reaches_core(key) and not reproduced_core):
            raise ValueError("transitive core/db feature has no supported forwarding path")
    unique = list(dict.fromkeys(flags))
    return [word for feature in unique for word in ("--features", feature)]
