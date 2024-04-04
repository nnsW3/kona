//! This module contains the `ChannelBank` struct.

use super::frame_queue::FrameQueue;
use crate::{
    params::{ChannelID, MAX_CHANNEL_BANK_SIZE},
    traits::{
        ChainProvider, DataAvailabilityProvider, LogLevel, ResettableStage, TelemetryProvider,
    },
    types::{BlockInfo, Channel, Frame, RollupConfig, StageError, StageResult, SystemConfig},
};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use alloy_primitives::Bytes;
use anyhow::anyhow;
use async_trait::async_trait;
use core::fmt::Debug;
use hashbrown::HashMap;

/// [ChannelBank] is a stateful stage that does the following:
/// 1. Unmarshalls frames from L1 transaction data
/// 2. Applies those frames to a channel
/// 3. Attempts to read from the channel when it is ready
/// 4. Prunes channels (not frames) when the channel bank is too large.
///
/// Note: we prune before we ingest data.
/// As we switch between ingesting data & reading, the prune step occurs at an odd point
/// Specifically, the channel bank is not allowed to become too large between successive calls
/// to `IngestData`. This means that we can do an ingest and then do a read while becoming too
/// large. [ChannelBank] buffers channel frames, and emits full channel data
#[derive(Debug)]
pub struct ChannelBank<DAP, CP, T>
where
    DAP: DataAvailabilityProvider + Debug,
    CP: ChainProvider + Debug,
    T: TelemetryProvider + Debug,
{
    /// The rollup configuration.
    cfg: Arc<RollupConfig>,
    /// Telemetry
    telemetry: T,
    /// Map of channels by ID.
    channels: HashMap<ChannelID, Channel>,
    /// Channels in FIFO order.
    channel_queue: VecDeque<ChannelID>,
    /// The previous stage of the derivation pipeline.
    prev: FrameQueue<DAP, CP, T>,
}

impl<DAP, CP, T> ChannelBank<DAP, CP, T>
where
    DAP: DataAvailabilityProvider + Debug,
    CP: ChainProvider + Debug,
    T: TelemetryProvider + Debug,
{
    /// Create a new [ChannelBank] stage.
    pub fn new(cfg: Arc<RollupConfig>, prev: FrameQueue<DAP, CP, T>, telemetry: T) -> Self {
        Self { cfg, telemetry, channels: HashMap::new(), channel_queue: VecDeque::new(), prev }
    }

    /// Returns the L1 origin [BlockInfo].
    pub fn origin(&self) -> Option<&BlockInfo> {
        self.prev.origin()
    }

    /// Returns the size of the channel bank by accumulating over all channels.
    pub fn size(&self) -> usize {
        self.channels.iter().fold(0, |acc, (_, c)| acc + c.size())
    }

    /// Prunes the Channel bank, until it is below [MAX_CHANNEL_BANK_SIZE].
    /// Prunes from the high-priority channel since it failed to be read.
    pub fn prune(&mut self) -> StageResult<()> {
        let mut total_size = self.size();
        while total_size > MAX_CHANNEL_BANK_SIZE {
            let id = self.channel_queue.pop_front().ok_or(anyhow!("No channel to prune"))?;
            let channel = self.channels.remove(&id).ok_or(anyhow!("Could not find channel"))?;
            total_size -= channel.size();
        }
        Ok(())
    }

    /// Adds new L1 data to the channel bank. Should only be called after all data has been read.
    pub fn ingest_frame(&mut self, frame: Frame) -> StageResult<()> {
        let origin = *self.origin().ok_or(anyhow!("No origin"))?;

        // Get the channel for the frame, or create a new one if it doesn't exist.
        let current_channel = self.channels.entry(frame.id).or_insert_with(|| {
            let channel = Channel::new(frame.id, origin);
            self.channel_queue.push_back(frame.id);
            channel
        });

        // Check if the channel is not timed out. If it has, ignore the frame.
        if current_channel.open_block_number() + self.cfg.channel_timeout < origin.number {
            self.telemetry.write(
                alloy_primitives::Bytes::from(alloc::format!("Channel {:?} timed out", frame.id)),
                LogLevel::Warning,
            );
            return Ok(());
        }

        // Ingest the frame. If it fails, ignore the frame.
        let frame_id = frame.id;
        if current_channel.add_frame(frame, origin).is_err() {
            self.telemetry.write(
                alloy_primitives::Bytes::from(alloc::format!(
                    "Failed to add frame to channel: {:?}",
                    frame_id
                )),
                LogLevel::Warning,
            );
            return Ok(());
        }

        self.prune()
    }

    /// Read the raw data of the first channel, if it's timed-out or closed.
    ///
    /// Returns an error if there is nothing new to read.
    pub fn read(&mut self) -> StageResult<Option<Bytes>> {
        // Bail if there are no channels to read from.
        if self.channel_queue.is_empty() {
            self.telemetry
                .write(alloy_primitives::Bytes::from("No channels to read from"), LogLevel::Debug);
            return Err(StageError::Eof);
        }

        // Return an `Ok(None)` if the first channel is timed out. There may be more timed
        // out channels at the head of the queue and we want to remove them all.
        let first = self.channel_queue[0];
        let channel = self.channels.get(&first).ok_or(anyhow!("Channel not found"))?;
        let origin = self.origin().ok_or(anyhow!("No origin present"))?;

        // Remove all timed out channels from the front of the `channel_queue`.
        if channel.open_block_number() + self.cfg.channel_timeout < origin.number {
            self.telemetry.write(
                alloy_primitives::Bytes::from(alloc::format!("Channel {:?} timed out", first)),
                LogLevel::Warning,
            );
            self.channels.remove(&first);
            self.channel_queue.pop_front();
            return Ok(None);
        }

        // At this point we have removed all timed out channels from the front of the
        // `channel_queue`. Pre-Canyon we simply check the first index.
        // Post-Canyon we read the entire channelQueue for the first ready channel.
        // If no channel is available, we return StageError::Eof.
        // Canyon is activated when the first L1 block whose time >= CanyonTime, not on the L2
        // timestamp.
        if !self.cfg.is_canyon_active(origin.timestamp) {
            return self.try_read_channel_at_index(0).map(Some);
        }

        let channel_data =
            (0..self.channel_queue.len()).find_map(|i| self.try_read_channel_at_index(i).ok());
        match channel_data {
            Some(data) => Ok(Some(data)),
            None => Err(StageError::Eof),
        }
    }

    /// Pulls the next piece of data from the channel bank. Note that it attempts to pull data out
    /// of the channel bank prior to loading data in (unlike most other stages). This is to
    /// ensure maintain consistency around channel bank pruning which depends upon the order
    /// of operations.
    pub async fn next_data(&mut self) -> StageResult<Option<Bytes>> {
        match self.read() {
            Err(StageError::Eof) => {
                // continue - we will attempt to load data into the channel bank
            }
            Err(e) => {
                return Err(anyhow!("Error fetching next data from channel bank: {:?}", e).into());
            }
            data => return data,
        };

        // Load the data into the channel bank
        let frame = self.prev.next_frame().await?;
        self.ingest_frame(frame)?;
        Err(StageError::NotEnoughData)
    }

    /// Attempts to read the channel at the specified index. If the channel is not ready or timed
    /// out, it will return an error.
    /// If the channel read was successful, it will remove the channel from the channel queue.
    fn try_read_channel_at_index(&mut self, index: usize) -> StageResult<Bytes> {
        let channel_id = self.channel_queue[index];
        let channel = self.channels.get(&channel_id).ok_or(anyhow!("Channel not found"))?;
        let origin = self.origin().ok_or(anyhow!("No origin present"))?;

        let timed_out = channel.open_block_number() + self.cfg.channel_timeout < origin.number;
        if timed_out || !channel.is_ready() {
            return Err(StageError::Eof);
        }

        let frame_data = channel.frame_data();
        self.channels.remove(&channel_id);
        self.channel_queue.remove(index);

        frame_data.map_err(StageError::Custom)
    }
}

#[async_trait]
impl<DAP, CP, T> ResettableStage for ChannelBank<DAP, CP, T>
where
    DAP: DataAvailabilityProvider + Send + Debug,
    CP: ChainProvider + Send + Debug,
    T: TelemetryProvider + Send + Debug,
{
    async fn reset(&mut self, _: BlockInfo, _: SystemConfig) -> StageResult<()> {
        self.channels.clear();
        self.channel_queue = VecDeque::with_capacity(10);
        Err(StageError::Eof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        stages::{
            frame_queue::tests::new_test_frames, l1_retrieval::L1Retrieval, l1_traversal::tests::*,
        },
        traits::test_utils::{TestDAP, TestTelemetry},
    };
    use alloc::vec;

    #[test]
    fn test_ingest_empty_origin() {
        let mut traversal = new_test_traversal(vec![], vec![]);
        traversal.block = None;
        let dap = TestDAP::default();
        let retrieval = L1Retrieval::new(traversal, dap, TestTelemetry::new());
        let frame_queue = FrameQueue::new(retrieval, TestTelemetry::new());
        let mut channel_bank =
            ChannelBank::new(Arc::new(RollupConfig::default()), frame_queue, TestTelemetry::new());
        let frame = Frame::default();
        let err = channel_bank.ingest_frame(frame).unwrap_err();
        assert_eq!(err, StageError::Custom(anyhow!("No origin")));
    }

    #[test]
    fn test_ingest_and_prune_channel_bank() {
        let traversal = new_populated_test_traversal();
        let results = vec![Ok(Bytes::from(vec![0x00]))];
        let dap = TestDAP { results };
        let retrieval = L1Retrieval::new(traversal, dap, TestTelemetry::new());
        let frame_queue = FrameQueue::new(retrieval, TestTelemetry::new());
        let mut channel_bank =
            ChannelBank::new(Arc::new(RollupConfig::default()), frame_queue, TestTelemetry::new());
        let mut frames = new_test_frames(100000);
        // Ingest frames until the channel bank is full and it stops increasing in size
        let mut current_size = 0;
        let next_frame = frames.pop().unwrap();
        channel_bank.ingest_frame(next_frame).unwrap();
        while channel_bank.size() > current_size {
            current_size = channel_bank.size();
            let next_frame = frames.pop().unwrap();
            channel_bank.ingest_frame(next_frame).unwrap();
            assert!(channel_bank.size() <= MAX_CHANNEL_BANK_SIZE);
        }
        // There should be a bunch of frames leftover
        assert!(!frames.is_empty());
        // If we ingest one more frame, the channel bank should prune
        // and the size should be the same
        let next_frame = frames.pop().unwrap();
        channel_bank.ingest_frame(next_frame).unwrap();
        assert_eq!(channel_bank.size(), current_size);
    }

    #[tokio::test]
    async fn test_read_empty_channel_bank() {
        let traversal = new_populated_test_traversal();
        let results = vec![Ok(Bytes::from(vec![0x00]))];
        let dap = TestDAP { results };
        let retrieval = L1Retrieval::new(traversal, dap, TestTelemetry::new());
        let frame_queue = FrameQueue::new(retrieval, TestTelemetry::new());
        let mut channel_bank =
            ChannelBank::new(Arc::new(RollupConfig::default()), frame_queue, TestTelemetry::new());
        let err = channel_bank.read().unwrap_err();
        assert_eq!(err, StageError::Eof);
        let err = channel_bank.next_data().await.unwrap_err();
        assert_eq!(err, StageError::NotEnoughData);
    }
}
