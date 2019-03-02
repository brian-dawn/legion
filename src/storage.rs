use crate::*;
use downcast_rs::{impl_downcast, Downcast};
use std::any::TypeId;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::mem::size_of;
use std::sync::atomic::AtomicIsize;
use std::sync::Arc;

impl_downcast!(ComponentStorage);
trait ComponentStorage: Downcast + Debug {
    fn remove(&mut self, id: ComponentID);
    fn len(&self) -> usize;
}

#[derive(Debug)]
struct UnsafeVec<T>(UnsafeCell<Vec<T>>);

impl<T: Debug> UnsafeVec<T> {
    fn with_capacity(capacity: usize) -> Self {
        UnsafeVec(UnsafeCell::new(Vec::<T>::with_capacity(capacity)))
    }

    unsafe fn inner(&self) -> &Vec<T> {
        &(*self.0.get())
    }

    unsafe fn inner_mut(&self) -> &mut Vec<T> {
        &mut (*self.0.get())
    }
}

impl<T: Debug + 'static> ComponentStorage for UnsafeVec<T> {
    fn remove(&mut self, id: ComponentID) {
        unsafe {
            self.inner_mut().swap_remove(id as usize);
        }
    }

    fn len(&self) -> usize {
        unsafe { self.inner_mut().len() }
    }
}

impl_downcast!(SharedComponentStorage);
trait SharedComponentStorage: Downcast + Debug {}

#[derive(Debug)]
struct SharedComponentStore<T>(UnsafeCell<T>);

impl<T: SharedData> SharedComponentStorage for SharedComponentStore<T> {}

#[derive(Debug)]
pub struct Chunk {
    capacity: usize,
    entities: UnsafeVec<Entity>,
    components: HashMap<TypeId, Box<dyn ComponentStorage>>,
    shared: HashMap<TypeId, Arc<dyn SharedComponentStorage>>,
    borrows: HashMap<TypeId, AtomicIsize>,
}

impl Chunk {
    pub fn len(&self) -> usize {
        unsafe { self.entities.inner().len() }
    }

    pub fn is_full(&self) -> bool {
        self.len() == self.capacity
    }

    pub unsafe fn entities(&self) -> &[Entity] {
        self.entities.inner()
    }

    pub unsafe fn entities_unchecked(&self) -> &mut Vec<Entity> {
        self.entities.inner_mut()
    }

    pub unsafe fn entity_data_unchecked<T: EntityData>(&self) -> Option<&mut Vec<T>> {
        self.components
            .get(&TypeId::of::<T>())
            .and_then(|c| c.downcast_ref::<UnsafeVec<T>>())
            .map(|c| c.inner_mut())
    }

    pub fn entity_data<'a, T: EntityData>(&'a self) -> Option<BorrowedSlice<'a, T>> {
        match unsafe { self.entity_data_unchecked() } {
            Some(data) => {
                let borrow = self.borrow::<T>();
                Some(BorrowedSlice::new(data, borrow))
            }
            None => None,
        }
    }

    pub fn entity_data_mut<'a, T: EntityData>(&'a self) -> Option<BorrowedMutSlice<'a, T>> {
        match unsafe { self.entity_data_unchecked() } {
            Some(data) => {
                let borrow = self.borrow_mut::<T>();
                Some(BorrowedMutSlice::new(data, borrow))
            }
            None => None,
        }
    }

    pub unsafe fn shared_component<T: SharedData>(&self) -> Option<&T> {
        self.shared
            .get(&TypeId::of::<T>())
            .and_then(|s| s.downcast_ref::<SharedComponentStore<T>>())
            .map(|s| &*s.0.get())
    }

    pub unsafe fn remove(&mut self, id: ComponentID) -> Option<Entity> {
        let index = id as usize;
        self.entities.inner_mut().swap_remove(index);
        for storage in self.components.values_mut() {
            storage.remove(id);
        }

        if self.entities.len() > index {
            Some(*self.entities.inner().get(index).unwrap())
        } else {
            None
        }
    }

    pub fn validate(&self) {
        let valid = self
            .components
            .values()
            .fold(true, |total, s| total && s.len() == self.entities.len());
        if !valid {
            panic!("imbalanced chunk components");
        }
    }

    fn borrow<'a, T: EntityData>(&'a self) -> Borrow<'a> {
        let id = TypeId::of::<T>();
        let state = self
            .borrows
            .get(&id)
            .expect("entity data type not found in chunk");
        Borrow::aquire_read(state).unwrap()
    }

    fn borrow_mut<'a, T: EntityData>(&'a self) -> Borrow<'a> {
        let id = TypeId::of::<T>();
        let state = self
            .borrows
            .get(&id)
            .expect("entity data type not found in chunk");
        Borrow::aquire_write(state).unwrap()
    }
}

pub struct ChunkBuilder {
    components: Vec<(
        TypeId,
        usize,
        Box<dyn FnMut(usize) -> Box<dyn ComponentStorage>>,
    )>,
    shared: HashMap<TypeId, Arc<dyn SharedComponentStorage>>,
}

impl ChunkBuilder {
    const MAX_SIZE: usize = 16 * 1024;

    pub fn new() -> ChunkBuilder {
        ChunkBuilder {
            components: Vec::new(),
            shared: HashMap::new(),
        }
    }

    pub fn register_component<T: EntityData>(&mut self) {
        let constructor = |capacity| {
            Box::new(UnsafeVec::<T>::with_capacity(capacity)) as Box<dyn ComponentStorage>
        };
        self.components
            .push((TypeId::of::<T>(), size_of::<T>(), Box::new(constructor)));
    }

    pub fn register_shared<T: SharedData>(&mut self, data: T) {
        self.shared.insert(
            TypeId::of::<T>(),
            Arc::new(SharedComponentStore(UnsafeCell::new(data)))
                as Arc<dyn SharedComponentStorage>,
        );
    }

    pub fn build(self) -> Chunk {
        let size_bytes = *self
            .components
            .iter()
            .map(|(_, size, _)| size)
            .max()
            .unwrap_or(&ChunkBuilder::MAX_SIZE);
        let capacity = std::cmp::max(1, ChunkBuilder::MAX_SIZE / size_bytes);
        Chunk {
            capacity: capacity,
            borrows: self
                .components
                .iter()
                .map(|(id, _, _)| (*id, AtomicIsize::new(0)))
                .collect(),
            entities: UnsafeVec::with_capacity(capacity),
            components: self
                .components
                .into_iter()
                .map(|(id, _, mut con)| (id, con(capacity)))
                .collect(),
            shared: self.shared,
        }
    }
}

#[derive(Debug)]
pub struct Archetype {
    logger: slog::Logger,
    pub components: HashSet<TypeId>,
    pub shared: HashSet<TypeId>,
    pub chunks: Vec<Chunk>,
}

impl Archetype {
    pub fn new(
        logger: slog::Logger,
        components: HashSet<TypeId>,
        shared: HashSet<TypeId>,
    ) -> Archetype {
        Archetype {
            logger,
            components,
            shared,
            chunks: Vec::new(),
        }
    }

    pub fn chunk(&self, id: ChunkID) -> Option<&Chunk> {
        self.chunks.get(id as usize)
    }

    pub fn chunk_mut(&mut self, id: ChunkID) -> Option<&mut Chunk> {
        self.chunks.get_mut(id as usize)
    }

    pub fn has_component<T: EntityData>(&self) -> bool {
        self.components.contains(&TypeId::of::<T>())
    }

    pub fn has_shared<T: SharedData>(&self) -> bool {
        self.shared.contains(&TypeId::of::<T>())
    }

    pub fn chunks(&self) -> impl Iterator<Item = &Chunk> {
        self.chunks.iter()
    }

    pub fn get_or_create_chunk<'a, 'b, 'c, S: SharedDataSet, C: ComponentSource>(
        &'a mut self,
        shared: &'b S,
        components: &'c C,
    ) -> (ChunkID, &'a mut Chunk) {
        match self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_full() && shared.is_chunk_match(c))
            .map(|(i, _)| i)
            .next()
        {
            Some(i) => (i as ChunkID, unsafe { self.chunks.get_unchecked_mut(i) }),
            None => {
                let mut builder = ChunkBuilder::new();
                shared.configure_chunk(&mut builder);
                components.configure_chunk(&mut builder);
                self.chunks.push(builder.build());

                let chunk_id = (self.chunks.len() - 1) as ChunkID;
                let chunk = self.chunks.last_mut().unwrap();

                debug!(self.logger, "allocated chunk"; "chunk_id" => chunk_id);

                (chunk_id, chunk)
            }
        }
    }
}
