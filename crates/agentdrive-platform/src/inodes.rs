use agentdrive_model::{Error, ErrorCode, InodeId, Result};

/// Kernel inode numbers remain unique across binding generations, including cached cwd references.
pub(crate) struct InodeNamespace {
    generation: u64,
    base: u64,
    highest_inode: u64,
}

impl InodeNamespace {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            base: 0,
            highest_inode: 1,
        }
    }

    fn advance(&mut self, generation: u64) -> Result<()> {
        if generation < self.generation {
            return Err(stale());
        }
        if generation != self.generation {
            // Reserve the entire previous range so inode translation needs constant memory.
            self.base = self
                .base
                .checked_add(self.highest_inode)
                .ok_or_else(exhausted)?;
            self.highest_inode = 1;
            self.generation = generation;
        }
        Ok(())
    }

    pub fn encode(&mut self, generation: u64, inode: InodeId) -> Result<u64> {
        self.advance(generation)?;
        if inode == InodeId::ROOT {
            return Ok(1);
        }
        if inode.0 == 0 {
            return Err(Error::integrity("workspace inode cannot be zero"));
        }
        let value = self.base.checked_add(inode.0).ok_or_else(exhausted)?;
        self.highest_inode = self.highest_inode.max(inode.0);
        Ok(value)
    }

    pub fn decode(&mut self, generation: u64, inode: u64) -> Result<InodeId> {
        self.advance(generation)?;
        if inode == 1 {
            return Ok(InodeId::ROOT);
        }
        match inode.checked_sub(self.base) {
            Some(value) if value > 1 && value <= self.highest_inode => Ok(InodeId(value)),
            _ => Err(stale()),
        }
    }
}

fn exhausted() -> Error {
    Error::new(ErrorCode::CapacityExceeded, "native inode space exhausted")
}

fn stale() -> Error {
    Error::new(
        ErrorCode::StaleBinding,
        "native inode belongs to a previous mount binding",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_inode_cannot_address_another_branch_after_rebind() {
        let mut namespace = InodeNamespace::new(10);
        let old = namespace.encode(10, InodeId(2)).unwrap();
        assert_eq!(namespace.encode(10, InodeId(2)).unwrap(), old);
        assert_eq!(namespace.decode(10, old).unwrap(), InodeId(2));
        let new = namespace.encode(11, InodeId(2)).unwrap();
        assert_ne!(old, new);
        assert_eq!(
            namespace.decode(11, old).unwrap_err().code,
            ErrorCode::StaleBinding
        );
        assert_eq!(namespace.decode(11, new).unwrap(), InodeId(2));
        assert_eq!(
            namespace.encode(10, InodeId(3)).unwrap_err().code,
            ErrorCode::StaleBinding
        );
        assert_eq!(
            namespace.decode(10, new).unwrap_err().code,
            ErrorCode::StaleBinding
        );
        assert_eq!(namespace.decode(11, 1).unwrap(), InodeId::ROOT);
        assert_eq!(namespace.encode(11, InodeId::ROOT).unwrap(), 1);
    }

    #[test]
    fn inode_ranges_preserve_sparse_ids_without_reusing_previous_ranges() {
        let mut namespace = InodeNamespace::new(1);
        let old = namespace.encode(1, InodeId(1_000_000)).unwrap();
        let next = namespace.encode(2, InodeId(2)).unwrap();
        assert!(next > old);
        assert_eq!(
            namespace.decode(2, old).unwrap_err().code,
            ErrorCode::StaleBinding
        );
        assert_eq!(namespace.decode(2, next).unwrap(), InodeId(2));
        let last = namespace
            .encode(2, InodeId(u64::MAX - namespace.base))
            .unwrap();
        assert_eq!(last, u64::MAX);
        assert_eq!(
            namespace.encode(3, InodeId(2)).unwrap_err().code,
            ErrorCode::CapacityExceeded
        );
    }
}
