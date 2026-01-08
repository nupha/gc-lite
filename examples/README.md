# GC-Lite Examples

This directory contains examples of the gc-lite, demonstrating various features and usage of the system.

## Files List

### 1. [basic_usage.rs](basic_usage.rs) - Basic Usage Example

**Purpose**: Demonstrates basic usage of the partitioned garbage collection system, suitable for beginners to understand core concepts.

**Main Content**:

- Create garbage collection context and partitions
- Allocate various types of objects in different partitions (strings, numbers, vectors, etc.)
- Set and clear root objects
- Manually trigger partition garbage collection
- Automatic garbage collection mechanism
- Creation and usage of weak references
- Complex type implementation with GC references
- Partition management (creation, deletion)

**Key Features**:

- Complete lifecycle management demonstration
- Circular reference handling
- Memory usage status monitoring
- Type registration and tracing mechanism

### 2. [advanced_features.rs](advanced_features.rs) - Advanced Features Example

**Purpose**: Demonstrates advanced features of the system and complex scenario handling capabilities.

**Main Content**:

- **Weak Reference Functionality**: Conversion between strong and weak references, weak reference upgrade verification
- **Circular Reference Handling**: Creation, management and collection of mutually referencing nodes
- **Complex Data Structures**: Complex object graphs such as tree structures and data containers
- **Reference Recovery Functionality**: Verification of the ability to recover GcRef from object references
- **Cross-Context Detection**: Object source verification in multi-context environments

**Key Features**:

- Automatic circular reference detection and collection
- Correct tracing of complex object graphs
- Type-safe reference recovery
- Cross-context security protection

### 3. [error_handling.rs](error_handling.rs) - Error Handling Example

**Purpose**: Demonstrates error handling mechanisms and boundary condition handling of the system.

**Main Content**:

- **Out of Memory Errors**: Partition memory limits and allocation failure handling
- **Safe Release Verification**:
  - Double release detection
  - Cross-context release protection
  - Referenced object release restrictions
- **Partition Management Errors**:
  - Non-existent partition operations
  - Non-empty partition deletion
- **GC Threshold API Errors**: Threshold setting validation and error handling

**Key Features**:

- Comprehensive error type coverage
- Safe memory management verification
- Robust partition operation testing
- API boundary condition validation

### 4. [performance_benchmark.rs](performance_benchmark.rs) - Performance Benchmark Example

**Purpose**: Evaluates system performance characteristics and memory usage efficiency.

**Main Content**:

- **Object Scale Performance**: Allocation and collection performance for objects of different magnitudes
- **Complex Object Graph Performance**: GC performance analysis for complex dependency relationships
- **Memory Usage Efficiency**: Memory overhead analysis for small object allocations
- **Automatic GC Threshold Performance**: Performance performance of threshold triggering mechanism

**Key Features**:

- Multi-scale performance benchmarking
- Memory usage efficiency analysis
- Complex scenario performance evaluation
- Automatic GC mechanism performance verification

## Running Examples

To run specific examples, use the following commands:

```bash
# Run basic usage example
cargo run --example basic_usage

# Run advanced features example
cargo run --example advanced_features

# Run error handling example
cargo run --example error_handling

# Run performance benchmark example
cargo run --example performance_benchmark
```

## Example Design Principles

1. **Completeness**: Each example demonstrates a complete functional workflow
2. **Independence**: Examples are independent of each other and can be run and understood separately
3. **Practicality**: Designed based on real usage scenarios with practical reference value
4. **Educational**: Contains detailed comments and explanations for easy learning and understanding

## Learning Suggestions

- **Beginners**: Start with `basic_usage.rs` to understand basic concepts
- **Advanced Users**: Study `advanced_features.rs` to master advanced features
- **System Integration**: Reference `error_handling.rs` to ensure robustness
- **Performance Optimization**: Use `performance_benchmark.rs` for performance analysis

These examples together form a complete usage guide for the GC-Lite system, covering various usage scenarios from basic to advanced.
