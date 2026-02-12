# Architecture

This page shows how the crate's components connect and where your domain code fits in.

## Component Overview

```d2
App: "Your Application" {
  Command
  Aggregate Definition
  Event Definitions
  Projection Definition
}

Crate: "sourcery crate" {
  Repository
  EventStore {shape: cylinder}
  SnapshotStore {shape: cylinder}

  Repository -> EventStore
  Repository -> SnapshotStore: {style.stroke-dash: 3}
}

App.Command -> Crate.Repository
App.Aggregate Definition -> Crate.Repository
App.Event Definitions -> Crate.Repository
App.Projection Definition -> Crate.Repository
```

**You define**: Aggregates, events, commands, projections

**The crate provides**: Repository orchestration, store abstraction, serialization

## The Envelope Pattern

Events travel with metadata, but domain types stay pure:

- **Aggregates** receive only the pure event—no IDs or metadata
- **Projections** receive the full envelope—aggregate ID and metadata included
- **Stores** persist the envelope but deserialize only what's needed

This keeps domain logic free of infrastructure concerns.

## Command Execution Flow

```d2
shape: sequence_diagram

App: Application
Repo: Repository (snapshots enabled)
Store: EventStore
Agg: Aggregate
Snap: SnapshotStore

App -> Repo: "execute_command(id, cmd, meta)"
Repo -> Snap: "load(id)?"
Snap -> Repo: "Optional snapshot" {style.stroke-dash: 3}
Repo -> Store: "load_events(filters)"
Store -> Repo: "Vec<StoredEvent>" {style.stroke-dash: 3}
Repo -> Agg: "replay events (apply)"
Repo -> Agg: "handle(command)"
Agg -> Repo: "Vec<Event>" {style.stroke-dash: 3}
Repo -> Store: "commit_events()"
Repo -> Snap: "offer_snapshot()?"
Repo -> App: "Ok(())" {style.stroke-dash: 3}
```

## Projection Loading Flow

```d2
shape: sequence_diagram

App: Application
Repo: Repository
Store: EventStore
Proj: Projection

App -> Repo: "load_projection::<P>(&instance_id)"
Repo -> Proj: "P::filters(&instance_id)"
Proj -> Repo: "Filters (event filters + handlers)" {style.stroke-dash: 3}
Repo -> Store: "load_events(filters)"
Store -> Repo: "Vec<StoredEvent>" {style.stroke-dash: 3}
Repo -> Proj: "P::init(&instance_id)"
Repo -> Proj: "apply_projection(id, event, meta) [For each event]"
Repo -> App: "Projection" {style.stroke-dash: 3}
```

Projections define their event filters centrally in the `Subscribable` trait. The repository calls `filters()` to determine which events to load, then replays them into the projection.

## Key Types

| Type | Role |
|------|------|
| `Repository<S>` | Orchestrates aggregates and projections (no snapshots) |
| `Repository<S, C, Snapshots<SS>>` | Snapshot-enabled repository orchestration |
| `EventStore` | Trait for event persistence |
| `SnapshotStore` | Trait for aggregate/projection snapshots |
| `Aggregate` | Trait for command-side entities |
| `Subscribable` | Base trait for event subscribers (projections) |
| `Projection` | Extends `Subscribable` with a `KIND` for snapshots |
| `ApplyProjection<E>` | Per-event handler for projections |
| `Filters` | Builder for event filter specs + handler closures |
| `DomainEvent` | Marker trait for event structs |

## Next

[Installation](../getting-started/installation.md) — Add the crate to your project
