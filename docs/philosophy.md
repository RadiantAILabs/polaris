---
notion_page: https://www.notion.so/radiant-ai/Philosophy-327afe2e695d808db6c0dc5021adf1bb
title: Polaris Core Philosophy
---

# Polaris Core Philosophy

## Why Polaris Exists

We believe that building performant AI agents is a design problem. The bottleneck is not compute, APIs, or infrastructure. It is discovering how an agent should behave for a given use case, and being able to change that behavior quickly when it turns out to be wrong.

Most agent frameworks ship with a fixed execution model and a set of opinions about how agents should work. This works when the use case aligns with that model. When it does not, the framework becomes the constraint.

Polaris provides composable primitives without prescribing how they should be assembled. There is no default execution loop. Agent behavior is constructed from small, replaceable parts, and the framework imposes no opinion on the result. Finding the right design requires rapid experimentation, and every decision in Polaris is evaluated against that principle. If a feature does not enable faster iteration on agent design, it does not belong in the framework.

## Core Architecture

### ECS-Inspired State and Behavior

Polaris separates behavior from state, borrowing from the [Entity Component System (ECS)](https://en.wikipedia.org/wiki/Entity_component_system) pattern used in game engines such as [Bevy](https://bevy.org). State lives in shared **resources** within a central registry, and behavior lives in **systems**, which are pure functions that declare what resources they need and are run by the framework.

This separation keeps state inspectable, makes systems testable in isolation, and allows new behavior to be added by registration rather than inheritance. It also enables compile-time verification. Input and output types on systems enforce valid data flow, and resource access patterns are validated through the type system.
For multi-agent scenarios, Polaris extends the single-world model with hierarchical contexts. Each agent receives its own context with isolated state while retaining access to shared global resources through a parent-child context chain.

### Graph-Based Execution

Agent logic in Polaris is expressed as a directed graph of async functions. **Nodes** represent units of work such as an LLM call, a tool invocation, or a decision point. **Edges** define control flow between them, whether sequential, conditional, parallel, or looping.

The graph is the agent. Its full topology is inspectable, individual nodes can be swapped, and control flow can be restructured by rewiring edges. Connections are verified before execution begins, so structural errors surface early rather than mid-run.

### Plugin Architecture

The Polaris server is a plugin orchestrator. Every capability, including logging, tracing, I/O, tool execution, memory, and LLM providers, is delivered through plugins that are registered at startup.

This means that every component is replaceable, testable in isolation, and optional. Any implementation can be swapped with minimal conflicts. This makes it practical to experiment with alternatives, run different configurations in parallel, and package stable designs as reusable modules

---

Each layer of this architecture serves the same end: reducing the distance between a design hypothesis and an observable result. Polaris does not prescribe how agents should be built. It provides the machinery for finding out.
