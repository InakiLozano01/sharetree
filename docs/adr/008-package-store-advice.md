# 008 Keep package store advice cheap

An add operation can point out that sharing a dependency store or compiler cache would save more than cloning the source checkout. Keep this path cheap. Detect ecosystems from tracked marker files and gather filesystem facts separately. Group pip markers with the enclosing uv project to avoid duplicate advice.

A package store can hardlink only within one filesystem. Resolve a store that does not exist yet on the nearest existing ancestor, which is where it will be created. For the quick add hint, leave an unknown pnpm store location implicit. pnpm typically creates a per-filesystem store if its default is on another device. `sharetree doctor` can ask pnpm for the exact path at the cost of starting Node.
