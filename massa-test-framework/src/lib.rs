use std::sync::{Arc, Condvar, Mutex};

use massa_hash::Hash;
use massa_models::{
    block::{Block, BlockSerializer, SecureShareBlock},
    block_header::{BlockHeader, BlockHeaderSerializer},
    block_id::BlockId,
    secure_share::SecureShareContent,
    slot::Slot,
};
use massa_signature::KeyPair;
use tracing_subscriber::filter::LevelFilter;

pub trait TestUniverse {
    type ForeignControllers;
    type Config: Default;

    fn new(controllers: Self::ForeignControllers, config: Self::Config) -> Self;

    fn initialize(&self) {
        let default_panic = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_panic(info);
            std::process::exit(1);
        }));
        use tracing_subscriber::prelude::*;
        let tracing_layer = tracing_subscriber::fmt::layer().with_filter(LevelFilter::DEBUG);
        tracing_subscriber::registry().with(tracing_layer).init();
    }

    fn create_block(keypair: &KeyPair) -> SecureShareBlock {
        let header = BlockHeader::new_verifiable(
            BlockHeader {
                current_version: 0,
                announced_version: None,
                slot: Slot::new(1, 0),
                parents: vec![
                    BlockId::generate_from_hash(Hash::compute_from("Genesis 0".as_bytes())),
                    BlockId::generate_from_hash(Hash::compute_from("Genesis 1".as_bytes())),
                ],
                operation_merkle_root: Hash::compute_from(&Vec::new()),
                endorsements: Vec::new(),
                denunciations: Vec::new(),
            },
            BlockHeaderSerializer::new(),
            keypair,
        )
        .unwrap();

        Block::new_verifiable(
            Block {
                header,
                operations: Default::default(),
            },
            BlockSerializer::new(),
            keypair,
        )
        .unwrap()
    }
}

pub struct Breakpoint(Arc<BreakpointInner>);

struct BreakpointInner {
    mutex: Mutex<bool>,
    condvar: Condvar,
}

impl Default for Breakpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl Breakpoint {
    pub fn new() -> Self {
        Self(Arc::new(BreakpointInner {
            mutex: Mutex::new(false),
            condvar: Condvar::new(),
        }))
    }

    pub fn get_trigger_handle(&self) -> Breakpoint {
        Breakpoint(self.0.clone())
    }

    pub fn wait(&self) {
        let mut started = self.0.mutex.lock().unwrap();
        while !*started {
            started = self.0.condvar.wait(started).unwrap();
        }
    }

    pub fn trigger(&self) {
        let mut started = self.0.mutex.lock().unwrap();
        *started = true;
        // We notify the condvar that the value has changed.
        self.0.condvar.notify_one();
    }
}
