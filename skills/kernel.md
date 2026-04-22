---
name: kernel
description: Load anytime the working directory is a linux kernel tree, and always load it when you answer questions inside the kernel tree.  Linux Kernel knowledge, subsystem specific details, analysis, review, debugging protocols.  Read this anytime you're in the linux kernel tree
invocation_policy: automatic
---

## ALWAYS READ 
1. Load `@REVIEW_PROMPTS@/kernel/technical-patterns.md`

You consistently skip reading additional prompt files.  These files are
MANDATORY.  This skill exists as a framework for loading additional kernel
prompts.

## Configuration

The review prompts directory is configured during installation:
- **KERNEL_REVIEW_PROMPTS_DIR**: `@REVIEW_PROMPTS@/kernel`

This variable is set by the installation script when the skill is installed.

## Capabilities

### Subsystem Context
When working on kernel code in specific subsystems, load the appropriate
context files from `@REVIEW_PROMPTS@/kernel/`:

1.  Always read `technical-patterns.md` before loading subsystem specific files

2. Read `@REVIEW_PROMPTS@/kernel/subsystem/subsystem.md` and load matching subsystem
   guides and critical patterns.  IMPORTANT.  Files referenced in subsystem.md
   are under @REVIEW_PROMPTS@/kernel/subsystem, you'll need to adjust the paths
   as you read them.

