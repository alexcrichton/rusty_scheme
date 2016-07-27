extern crate libc;
use std::fs::File;
use std::mem;
use std::ptr;
use std::slice;
use super::value;
use value::{Value, SIZEOF_PAIR, HEADER_TAG, PAIR_HEADER, SYMBOL_TAG};
use symbol;
use bytecode;
#[cfg(debug_assertions)]
use value::Tags;
/// An allocator for RustyScheme objects
pub trait Allocator {
    /// Allocates a vector
    fn alloc_vector(&mut self, &[Value]) -> value::Vector;

    /// Allocates a pair
    fn alloc_pair(&mut self, car: Value, cdr: Value);

    /// Allocates a closure
    fn alloc_closure(&mut self, bytecode: &value::BCO, upvalues: &[Value]) -> value::Closure;

    /// Allocates a record
    fn alloc_record(&mut self,
                    descriptor: &value::RecordDescriptor,
                    fields: &[Value])
                    -> value::Record;

    /// Allocates a hash table
    fn alloc_hash_table(&mut self, size: usize) -> value::HashTable;

    /// Allocates a port
    fn alloc_port(&mut self, File) -> value::IOPort;

    /// Allocates a rustdata, which contains an arbitrary Rust object
    fn alloc_rustdata<T>(&mut self, object: &T) -> value::RustData;

// /// Allocates a boxed float on the top of the stack.
// fn alloc_float(&mut self, float: f64) -> value::Float;
}

/// An instance of the garbage-collected Scheme heap.
#[derive(Debug)]
pub struct Heap {
    /// The tospace.
    tospace: Vec<Value>,

    /// The fromspace.
    fromspace: Vec<Value>,

    /// The symbol table
    pub symbol_table: symbol::SymbolTable,

    /// The environment of the current closure.
    pub environment: *mut value::Vector,

    /// The constants vector of the current closure.
    pub constants: *const value::Vector,

    /// The execution stack.
    pub stack: self::Stack,
}

#[repr(packed)]
pub struct FinalizedObject {
    /// The standard header
    header: usize,

    /// Size of user struct
    user_struct_size: usize,

    /// Link in finalized object chain
    link: *const FinalizedObject,

    /// User payload
    payload: Drop,
}

use std::cell;

/// A GC root.
///
/// A `Root` is a handle to an object on the garbage-collected Scheme heap.
/// The Scheme garbage collector knows about `Root`s, and ensures that they
/// stay valid even when a garbage collection occurs.  Therefore, client code
/// must use `Root`s (or the stack) to store all references to Scheme data.
///
/// There are 2 types of `Root`: local roots and persistent roots.  A local
/// root can only be created from within a callback from the VM and is stack
/// allocated.  A global root can be created by any code that has a valid
/// reference to the VM, and may live for as long as the VM is alive.  Global
/// roots are more expensive in terms of VM resources (they are stored in a
/// heap-allocated data structure), but are not limited by a scope.
#[derive(Debug)]
pub struct Root<'a> {
    contents: &'a cell::UnsafeCell<Value>,
}

/// Consistency checks on the whole heap (in debug mode only) – sloooow.
#[cfg(debug_assertions)]
unsafe fn consistency_check(heap: &Vec<Value>) {
    use symbol;
    let mut index = 0;
    while index < heap.len() {
        let mut current = heap[index].clone();
        let len = current.get() as usize & !HEADER_TAG;
        assert!(len > 0);
        index += 1;
        for x in 1..len {
            current = heap[index].clone();
            match current.tag() {
                Tags::Num | Tags::Num2 => {
                    assert!(current.get() & 0b11 == 0);
                }
                Tags::Pair => {
                    assert!(current.get() & 0b111 == 0b111);
                    debug_assert_valid_value(&heap, &current);
                    if (*current.as_ptr()).get() != PAIR_HEADER {
                        bug!("BAD PAIR: header length is \
                                0x{:x} and not \
                                0x{:x} at index 0x{:x} into heap and index \
                                0x{:x} into block",
                             ((*current.as_ptr()).get()),
                             PAIR_HEADER,
                             index,
                             x);
                    }
                    for i in 1..3 {
                        debug_assert_valid_value(heap,
                                                 &*(current.as_ptr().offset(i as isize) as *const Value))
                    }
                }
                Tags::Vector => {
                    assert!(len > 0);
                    debug_assert_valid_value(heap, &current);
                    for i in 1..len {
                        debug_assert_valid_value(heap,
                                                 &*current.as_ptr().offset(i as isize))
                    }
                }
                Tags::Symbol => {
                    assert!(len == size_of!(symbol::Symbol) / size_of!(usize));
                    debug_assert_valid_value(heap, &current);
                    debug_assert_valid_value(heap, &*current.as_ptr().offset(1))
                }
                _ => unimplemented!(),
            }
            index += 1;
        }
    }
}

#[cfg(not(debug_assertions))]
unsafe fn consistency_check(_heap: &Vec<Value>) {}

/// Rounds the size of a heap object up to the nearest multiple of 8 bytes,
/// expressed in words.
fn align_word_size(size: usize) -> usize {
    let size_of_value = ::std::mem::size_of::<Value>();
    debug_assert!((!0b1usize).wrapping_add(2) == 0);
    let x = match size_of_value {
        4 => size + 1 & !0b1,
        8 => size,
        _ => ((size * size_of_value + 0b111) & !0b111) / size_of_value,
    };
    debug_assert!((x + 1) & !1 >= x);
    // assert!(x - size_of_value <= 1);
    x
}

/// Relocates a `Value` in the heap.
///
/// This function relocates a `Value` in the Scheme heap.  It takes two
/// arguments: `current`, the `Value` being relocated, and `end`, the current
/// end of tospace.
///
/// This function takes raw pointers because of aliasing concerns.
unsafe fn relocate(current: *mut Value, tospace: &mut Vec<Value>, fromspace: &mut Vec<Value>) {
    debug_assert!(tospace.capacity() >= fromspace.len());
    if false {
        debug!("Tospace capacity: {}, Fromspace length: {}",
               tospace.capacity(),
               fromspace.len());
    }
    let size_of_value: usize = size_of!(Value);
    (*current).size().map(|size| {
        // pointer to head of object being copied
        let pointer: *mut Value = (*current).as_ptr();

        debug!("HEADER_TAG is {:b}\n", HEADER_TAG);

        let header = (*pointer).get().clone();
        // Assert that the object header is nonzero.
        debug_assert!(header != 0,
                      "internal error: copy_value: invalid object header size");
        if header & HEADER_TAG == HEADER_TAG {
            debug_assert!(header == HEADER_TAG,
                          "Bad header: {ptr:x}\n", ptr = header);
            // Forwarding pointer detected (this header tag is otherwise absurd,
            // since no object can have a size of zero).
            *current = (&*pointer.offset(1)).clone()
        } else{
            let len = tospace.len();

            // End pointer
            let end = tospace.as_mut_ptr().offset(len as isize);

            let amount_to_copy = align_word_size(size);

            // Check that the amount to copy is reasonable
            debug_assert!(amount_to_copy > 0,
                          "internal error: relocate: zero-sized word");

            // Check that the end pointer is aligned
            debug_assert!(end as usize & 0b111 == 0,
                          "internal error: relocate: misaligned end pointer");

            // Check that the pointer really is to fromspace
            debug_assert!((pointer as usize) <
                          fromspace.as_ptr() as usize + fromspace.len() * size_of!(usize),
                          "internal error: relocate: attempt to relocate pointer not to fromspace");
            debug_assert!(pointer as usize >= fromspace.as_ptr() as usize);

            if cfg!(feature = "memcpy-gc") {
                let words_to_copy = amount_to_copy * size_of_value;
                // The amount to copy
                debug_assert!(amount_to_copy + len <= tospace.capacity());
                debug_assert!(pointer as usize >= end as usize + words_to_copy ||
                              pointer as usize + words_to_copy <= end as usize);
                // NOTE: reverse pointer argument order from `memcpy`.
                ptr::copy_nonoverlapping(pointer, end, amount_to_copy);
                tospace.set_len(len + amount_to_copy)
            } else {
                // NOTE: this MUST come before replacing the old object with
                // a forwarding pointer – otherwise, this replacement will
                // clobber the copied object's header!
                tospace.extend_from_slice(slice::from_raw_parts(pointer, amount_to_copy));
            }
            *pointer = Value::new(HEADER_TAG);
            *current = Value::new(end as usize | ((*current).get() & 0b111));
            *pointer.offset(1) = (*current).clone();
        }
    });
}

/// Process the heap.
unsafe fn scavange_heap(tospace: &mut Vec<Value>, fromspace: &mut Vec<Value>) {
    let mut offset: isize = 0;
    use std::isize;
    assert!(tospace.len() <= isize::MAX as usize);
    assert!(fromspace.len() <= isize::MAX as usize);
    let current = tospace.as_mut_ptr();
    while offset < tospace.len() as isize {
        let header = (*current.offset(offset)).get();
        let size = header & !HEADER_TAG;
        let tag = header & HEADER_TAG;
        assert!(size > 0);
        offset += 1;
        match tag {
            value::HEADER_TAG => /* Forwarding pointer */
                bug!("Forwarding pointer in tospace"),
            value::PAIR_HEADER_TAG => /* Pair */ {
                debug_assert!(size == 3)
            }
            value::SYMBOL_HEADER_TAG => /* Symbol */ {
                relocate(current.offset(offset), tospace, fromspace);
                offset += size as isize - 1;
                continue;
            }
            value::RUSTDATA_HEADER_TAG => /* Rustdata – not scanned by the GC */ {
                offset += size as isize - 1;
                continue;
            }
            value::VECTOR_HEADER_TAG => /* Vector-like object */ { }
            value::BYTECODE_HEADER_TAG => /* Bytecode object */ {
                let ptr: *mut bytecode::BCO = current.offset(-1) as *mut _;
                relocate(&mut(*ptr).constants_vector, tospace, fromspace);
                offset += size as isize - 1;
                continue;
            }
            _ => bug!("Strange header type {:x}", tag)
        }

        if !(*current).leafp() {
            if !(*current).raw_tag() != SYMBOL_TAG {
                for _ in 1..size {
                    relocate(current.offset(offset), tospace, fromspace);
                    offset += 1
                }
            } else {
                relocate(current.offset(offset), tospace, fromspace);
                offset += size as isize - 1
            }
            offset = align_word_size(offset as usize) as isize
        }
    }
}

/// Handles all of the data on the stack.
unsafe fn scavange_stack(stack: &mut Vec<Value>,
                         tospace: &mut Vec<Value>,
                         fromspace: &mut Vec<Value>) {
    for i in stack.iter_mut() {
        relocate(i, tospace, fromspace);
    }
}

/// Performs a full garbage collection
fn collect(heap: &mut Heap) {
    debug!("Initiated garbage collection");
    unsafe {
        if cfg!(debug_assertions) {
            for i in &heap.stack.innards {
                debug_assert_valid_value(&heap.tospace, i)
            }
        }
        consistency_check(&heap.tospace);
        debug!("Completed first consistency check");
        mem::swap(&mut heap.tospace, &mut heap.fromspace);
        heap.tospace.reserve(heap.fromspace.len() + heap.fromspace.len() / 2);
        debug!("Fromspace size is {}",
               heap.fromspace.len() + heap.fromspace.len() / 2);
        heap.tospace.resize(0, Value::new(0));
        debug!("Tospace resized to {}", heap.tospace.capacity());
        scavange_stack(&mut heap.stack, &mut heap.tospace, &mut heap.fromspace);
        debug!("Stack scavanged");
        scavange_heap(&mut heap.tospace, &mut heap.fromspace);
        debug!("Heap scavanged");
        heap.symbol_table.fixup();
        debug!("Fixed up symbol table");
        if cfg!(debug_assertions) {
            for i in &heap.stack.innards {
                debug_assert_valid_value(&heap.tospace, i)
            }
        }
        consistency_check(&heap.tospace);
        debug!("Completed second consistency check");
        heap.fromspace.resize(0, Value::new(0));
    }
}

/// Represents the stack.
#[derive(Debug)]
pub struct Stack {
    pub innards: Vec<value::Value>,
}

use std::ops::{Deref, DerefMut};

/// A `Stack` acts like a `Vec`.
impl Deref for Stack {
    type Target = Vec<value::Value>;
    fn deref(&self) -> &Vec<value::Value> {
        &self.innards
    }
}

/// A `Stack` acts like a `Vec`.
impl DerefMut for Stack {
    fn deref_mut<'a>(&'a mut self) -> &'a mut Vec<value::Value> {
        &mut self.innards
    }
}

/// Assert a value is valid (in debug mode)
fn debug_assert_valid_value(vec: &Vec<Value>, i: &Value) {
    if cfg!(debug_assertions) {
        let lower_limit = vec.as_ptr() as usize;
        let upper_limit = lower_limit + vec.len() * size_of!(usize);
        let contents = i.contents.get();
        let untagged = contents & !0b111;
        if !(contents & 0b11 == 0 || (untagged >= lower_limit && untagged < upper_limit)) {
            let contents = contents;
            bug!("internal error: argument not fixnum or pointing into \
                  tospace: {:x}",
                 contents)
        }
    }
}

impl Heap {
    /// Allocates a Scheme pair, which must be rooted by the caller.
    ///
    /// The arguments are stack indexes.
    pub fn alloc_pair(&mut self, car: usize, cdr: usize) {
        if cfg!(debug_assertions) {
            for i in &[car, cdr] {
                debug_assert_valid_value(&self.tospace, &self.stack[*i])
            }
        }
        // unsafe { consistency_check(&self.tospace) }
        self.alloc_raw(SIZEOF_PAIR, value::PAIR_HEADER_TAG);
        let len = if size_of!(usize) < 8 {
            self.tospace.extend_from_slice(&[self.stack[car].clone(),
                                             self.stack[cdr].clone(),
                                             Value::new(1)]);
            self.tospace.len() - 4
        } else {
            self.tospace.extend_from_slice(&[self.stack[car].clone(),
                                             self.stack[cdr].clone()]);
            self.tospace.len() - 3
        };
        let new_value = Value::new(unsafe {
            self.tospace.as_ptr().offset(len as isize) as usize | value::PAIR_TAG
        });
        debug_assert_valid_value(&self.tospace, &new_value);
        self.stack.push(new_value);
        // unsafe { consistency_check(&self.tospace) }
        // debug!("Allocated a pair")
    }

    /// FIXME use enum for tag
    pub fn alloc_raw(&mut self, space: usize, tag: usize)
                     -> (*mut libc::c_void, usize) {
        let real_space = align_word_size(space);
        let tospace_space = self.tospace.capacity() - self.tospace.len();
        if tospace_space < real_space {
            collect(self);
        }
        let alloced_ptr = self.tospace.as_ptr() as usize + self.tospace.len();
        self.tospace.push(Value::new(space | tag));
        (alloced_ptr as *mut libc::c_void,
         align_word_size(self.tospace.len() + real_space))
    }

    /// Allocates a vector.  The `elements` array must be rooted for the GC.
    pub fn alloc_vector(&mut self, elements: &[Value]) {
        let (value_ptr, final_len) = self.alloc_raw(elements.len() + 2,
                                                    value::VECTOR_HEADER_TAG);
        self.tospace.push(Value::new(0));
        let ptr = value_ptr as usize | value::VECTOR_TAG;
        self.tospace.extend_from_slice(elements);
        unsafe { self.tospace.set_len(final_len) };
        self.stack.push(Value::new(ptr));
    }

    /// Allocates a closure. `src` and `src2` are as found in the opcode.
    pub fn alloc_closure(&mut self, src: u8, src2: u8, upvalues: usize) {
        let argcount = (src as u16) << 7 | src2 as u16;
        let vararg = src & ::std::i8::MIN as u8 == 0;
        let stack_len = self.stack.len();
        let (value_ptr, final_len) = self.alloc_raw(upvalues + 2, value::VECTOR_HEADER_TAG);
        let ptr = {
            let elements = &self.stack[stack_len - upvalues..stack_len];
            let ptr = value_ptr as usize | value::VECTOR_TAG;
            self.tospace.push(Value::new((argcount as usize) << 2 |
                                         (-(vararg as isize) as usize &
                                          ::std::isize::MIN as usize)));
            self.tospace.extend_from_slice(elements);
            unsafe { self.tospace.set_len(final_len) };
            ptr
        };
        self.stack.push(Value::new(ptr));
    }

    /// Create an instance of the garage collector
    pub fn new(size: usize) -> Self {
        Heap {
            fromspace: Vec::with_capacity(size),
            tospace: Vec::with_capacity(size),
            symbol_table: symbol::SymbolTable::new(),
            environment: ptr::null_mut(),
            constants: ptr::null(),
            stack: Stack { innards: Vec::with_capacity(1 << 16) },
        }
    }

    /// Interns a symbol.  TODO doesn't actually intern it.
    pub fn intern(&mut self, string: String) {
        //match self.symbol_table.get(string)
        unsafe {
            let (ptr, _) = self.alloc_raw(size_of!(symbol::Symbol),
                                          value::SYMBOL_HEADER_TAG);
            let value = Value::new(ptr as usize);
            let ptr = ptr as *mut symbol::Symbol;
            (*ptr).name = string;
            (*ptr).value = self.stack.pop().unwrap();
            self.stack.push(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use value::*;
    use std::cell::Cell;
    #[test]
    fn can_allocate_objects() {
        let zero: Value = Value { contents: Cell::new(0) };
        let mut heap = Heap::new(1 << 4);
        super::collect(&mut heap);
        debug!("HEADER_TAG = {:x}, PAIR_TAG = {:x}, SIZEOF_PAIR = {:x}",
               HEADER_TAG,
               PAIR_HEADER,
               SIZEOF_PAIR);
        heap.stack.push(zero);
        heap.alloc_pair(0, 0);
        heap.stack[0] = heap.stack.pop().unwrap();
        // debug!("{:?}", heap);
        for i in 1..((1 << 11)) {
            heap.alloc_pair(0, 0);
            assert_eq!(heap.stack.len(), 2);
            assert_eq!(heap.stack[1].tag(), Tags::Pair);
            heap.stack[0] = heap.stack.pop().unwrap();
            let assert_valid = |heap: &Heap| {
                let ref new_pair = heap.stack[0];
                assert_eq!(heap.stack[0].tag(), Tags::Pair);
                assert_eq!(new_pair.size(), Some(3));
                if let Kind::Pair(ptr) = new_pair.kind() {
                    assert_eq!(unsafe { (*ptr).car.tag() }, Tags::Pair);
                    assert_eq!(unsafe { (*ptr).cdr.tag() }, Tags::Pair)
                } else {
                    unreachable!()
                }
            };
            assert_valid(&heap);
            // super::collect(&mut heap);
            assert_valid(&heap);
            assert!(heap.tospace.len() >= 3 * i)
        }
        heap.stack.pop();
        assert!(heap.stack.len() == 0);
        // assert!(heap.fromspace.capacity() > 3* (1 << 20));
        // debug!("{:?}", heap);
        super::collect(&mut heap);
        assert!(heap.tospace.len() == 0)
    }
}
