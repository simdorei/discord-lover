//! Bounded, memory-only topology publication before prompt publication.
//! Watch coalescing is safe: consumers read the whole current index, not deltas.
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::sync::watch;
use twilight_model::{channel::Channel, gateway::event::Event};

const CAPACITY: usize = 65_536;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelTopology {
    pub guild: Option<u64>,
    pub parent: Option<u64>,
    pub deleted: bool,
}
#[derive(Debug, Default)]
pub struct TopologySnapshot {
    pub epoch: u64,
    pub revision: u64,
    pub channels: HashMap<u64, ChannelTopology>,
}
#[derive(Clone, Debug)]
pub struct GatewayTopology {
    state: Arc<RwLock<TopologySnapshot>>,
    changed: watch::Sender<u64>,
}
impl Default for GatewayTopology {
    fn default() -> Self {
        Self {
            state: Arc::new(RwLock::new(TopologySnapshot::default())),
            changed: watch::channel(0).0,
        }
    }
}
impl GatewayTopology {
    pub fn read<T>(&self, read: impl FnOnce(&TopologySnapshot) -> T) -> Option<T> {
        self.state.read().ok().map(|s| read(&s))
    }
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
    pub fn invalidate(&self) {
        if let Ok(mut state) = self.state.write() {
            state.epoch = state.epoch.saturating_add(1);
            state.revision = state.revision.saturating_add(1);
            state.channels.clear();
            self.changed.send_replace(state.revision);
        }
    }
    fn put(&self, id: u64, channel: ChannelTopology) {
        if let Ok(mut state) = self.state.write() {
            if state.channels.len() >= CAPACITY && !state.channels.contains_key(&id) {
                state.epoch = state.epoch.saturating_add(1);
                state.channels.clear();
            }
            state.channels.insert(id, channel);
            state.revision = state.revision.saturating_add(1);
            self.changed.send_replace(state.revision);
        }
    }
    fn channel(&self, channel: &Channel, deleted: bool) {
        self.put(
            channel.id.get(),
            ChannelTopology {
                guild: channel.guild_id.map(twilight_model::id::Id::get),
                parent: channel.parent_id.map(twilight_model::id::Id::get),
                deleted,
            },
        );
    }
    pub(super) fn publish(&self, event: &Event) {
        match event {
            Event::ChannelCreate(c) => self.channel(&c.0, false),
            Event::ChannelUpdate(c) => self.channel(&c.0, false),
            Event::ChannelDelete(c) => self.channel(&c.0, true),
            Event::ThreadCreate(c) => self.channel(&c.0, false),
            Event::ThreadUpdate(c) => self.channel(&c.0, false),
            Event::ThreadDelete(c) => self.put(
                c.id.get(),
                ChannelTopology {
                    guild: Some(c.guild_id.get()),
                    parent: Some(c.parent_id.get()),
                    deleted: true,
                },
            ),
            Event::ThreadListSync(sync) => {
                // A list can omit inaccessible/archived threads; never infer ownership removal.
                for channel in &sync.threads {
                    self.channel(channel, false);
                }
            }
            Event::Ready(_)
            | Event::Resumed
            | Event::GatewayReconnect
            | Event::GatewayInvalidateSession(_)
            | Event::GatewayClose(_)
            | Event::GatewayHello(_) => self.invalidate(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_and_capacity_loss_invalidate_old_negative_evidence() {
        let feed = GatewayTopology::default();
        let c = ChannelTopology {
            guild: Some(1),
            parent: Some(2),
            deleted: false,
        };
        feed.put(3, c);
        assert_eq!(feed.read(|s| s.channels[&3]), Some(c));
        feed.publish(&Event::Resumed);
        assert!(
            feed.read(|s| s.epoch == 1 && s.channels.is_empty())
                .unwrap()
        );
        for id in 1..=u64::try_from(CAPACITY + 1).unwrap() {
            feed.put(id, c);
        }
        assert!(
            feed.read(|s| s.epoch == 2 && s.channels.len() == 1)
                .unwrap()
        );
    }
}
