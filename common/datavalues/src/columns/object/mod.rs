// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod iterator;
mod mutable;

use std::sync::Arc;

use common_arrow::arrow::array::Array;
use common_arrow::arrow::bitmap::Bitmap;
use common_arrow::arrow::buffer::Buffer;
pub use iterator::*;
pub use mutable::*;
use serde_json::Value as JsonValue;

use crate::prelude::*;

/// ObjectColumn is a generic struct that wrapped any structure or enumeration,
/// such as JsonValue or BitMap.
#[derive(Clone)]
pub struct ObjectColumn<T: ObjectType> {
    values: Vec<T>,
}

impl<T: ObjectType> From<LargeBinaryArray> for ObjectColumn<T> {
    fn from(array: LargeBinaryArray) -> Self {
        Self::new(array)
    }
}

impl<T: ObjectType> ObjectColumn<T> {
    pub fn new(array: LargeBinaryArray) -> Self {
        let mut values: Vec<T> = Vec::with_capacity(array.len());
        let offsets = array.offsets().as_slice();
        let array_values = array.values().as_slice();
        for i in 0..offsets.len() - 1 {
            if let Some(validity) = array.validity() {
                if let Some(is_set) = validity.get(i) {
                    if !is_set {
                        values.push(T::default());
                        continue;
                    }
                }
            }
            let off = offsets[i] as usize;
            let len = (offsets[i + 1] - offsets[i]) as usize;
            let val = std::str::from_utf8(&array_values[off..off + len]).unwrap();
            match T::from_str(val) {
                Ok(v) => values.push(v),
                Err(_) => values.push(T::default()),
            }
        }

        Self { values }
    }

    pub fn from_arrow_array(array: &dyn Array) -> Self {
        let array = array
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .expect("object cast should be ok");

        Self::new(array.clone())
    }

    /// # Safety
    /// Assumes that the `i < self.len`.
    #[inline]
    pub unsafe fn value_unchecked(&self, i: usize) -> T {
        // soundness: the invariant of the function
        self.values.get_unchecked(i).clone()
    }

    pub fn values(&self) -> &[T] {
        self.values.as_slice()
    }

    /// Create a new DataArray by taking ownership of the Vec. This operation is zero copy.
    pub fn new_from_vec(values: Vec<T>) -> Self {
        Self { values }
    }
}

impl<T: ObjectType> Column for ObjectColumn<T> {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn data_type(&self) -> DataTypePtr {
        T::data_type()
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn validity(&self) -> (bool, Option<&Bitmap>) {
        (false, None)
    }

    fn memory_size(&self) -> usize {
        self.values.len() * std::mem::size_of::<T>()
    }

    fn as_arrow_array(&self) -> common_arrow::arrow::array::ArrayRef {
        let mut offsets: Vec<i64> = Vec::with_capacity(self.values.len());
        let mut values: Vec<u8> = Vec::with_capacity(self.values.len());

        let mut offset: i64 = 0;
        offsets.push(offset);
        for val in &self.values {
            let v = val.to_string();
            values.extend(v.as_bytes().to_vec());
            offset += v.len() as i64;
            offsets.push(offset);
        }

        Arc::new(LargeBinaryArray::from_data(
            self.data_type().arrow_type(),
            Buffer::from_slice(offsets),
            Buffer::from_slice(values),
            None,
        ))
    }

    fn arc(&self) -> ColumnRef {
        Arc::new(self.clone())
    }

    fn slice(&self, offset: usize, length: usize) -> ColumnRef {
        let values = &self.values.clone()[offset..offset + length];
        Arc::new(Self {
            values: values.to_vec(),
        })
    }

    fn filter(&self, filter: &BooleanColumn) -> ColumnRef {
        let length = filter.values().len() - filter.values().null_count();
        if length == self.len() {
            return Arc::new(self.clone());
        }
        let iter = self
            .values()
            .iter()
            .zip(filter.values().iter())
            .filter(|(_, f)| *f)
            .map(|(v, _)| v.clone());

        let values: Vec<T> = iter.collect();
        let col = ObjectColumn { values };

        Arc::new(col)
    }

    fn scatter(&self, indices: &[usize], scattered_size: usize) -> Vec<ColumnRef> {
        let mut builders = Vec::with_capacity(scattered_size);
        for _i in 0..scattered_size {
            builders.push(MutableObjectColumn::<T>::with_capacity(self.len()));
        }

        indices
            .iter()
            .zip(self.values())
            .for_each(|(index, value)| {
                builders[*index].append_value(value.clone());
            });

        builders.iter_mut().map(|b| b.to_column()).collect()
    }

    fn replicate(&self, offsets: &[usize]) -> ColumnRef {
        debug_assert!(
            offsets.len() == self.len(),
            "Size of offsets must match size of column"
        );

        if offsets.is_empty() {
            return self.slice(0, 0);
        }

        let mut builder =
            MutableObjectColumn::<T>::with_capacity(*offsets.last().unwrap() as usize);

        let mut previous_offset: usize = 0;

        (0..self.len()).for_each(|i| {
            let offset: usize = offsets[i];
            let data = unsafe { self.value_unchecked(i) };
            builder
                .values
                .extend(std::iter::repeat(data).take(offset - previous_offset));
            previous_offset = offset;
        });
        builder.to_column()
    }

    fn convert_full_column(&self) -> ColumnRef {
        Arc::new(self.clone())
    }

    fn get(&self, index: usize) -> DataValue {
        self.values[index].clone().into()
    }
}

impl<T> ScalarColumn for ObjectColumn<T>
where T: Scalar<ColumnType = Self> + ObjectType
{
    type Builder = MutableObjectColumn<T>;
    type OwnedItem = T;
    type RefItem<'a> = <T as Scalar>::RefType<'a>;
    type Iterator<'a> = ObjectValueIter<'a, T>;

    #[inline]
    fn get_data(&self, idx: usize) -> Self::RefItem<'_> {
        self.values[idx].as_scalar_ref()
    }

    fn scalar_iter(&self) -> Self::Iterator<'_> {
        ObjectValueIter::new(self)
    }
}

pub type JsonColumn = ObjectColumn<JsonValue>;

impl<T: ObjectType> std::fmt::Debug for ObjectColumn<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO(b41sh): implement display_fmt
        write!(f, "ObjectColumn")
    }
}
