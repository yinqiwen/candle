use super::{GgmlDType, QStorage};
use crate::quantized::k_quants::GgmlType;
use crate::{backend::BackendDevice, cuda_backend::WrapErr};
use crate::{CudaDevice, CudaStorage, Result};

use cudarc::driver::{CudaSlice, CudaView, DeviceSlice};

#[derive(Clone, Debug)]
pub struct QCudaStorage {
    data: CudaSlice<u8>,
    dtype: GgmlDType,
    device: CudaDevice,
}

static FORCE_DMMV: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_force_dmmv(f: bool) {
    FORCE_DMMV.store(f, std::sync::atomic::Ordering::Relaxed)
}

pub const WARP_SIZE: usize = 32;
pub const MMQ_X_Q4_0_AMPERE: usize = 4;
pub const MMQ_Y_Q4_0_AMPERE: usize = 32;
pub const NWARPS_Q4_0_AMPERE: usize = 4;
pub const GGML_CUDA_MMV_X: usize = 32;
pub const GGML_CUDA_MMV_Y: usize = 1;
pub const CUDA_QUANTIZE_BLOCK_SIZE: usize = 256;
pub const CUDA_DEQUANTIZE_BLOCK_SIZE: usize = 256;
pub const MATRIX_ROW_PADDING: usize = 512;

fn ceil_div(p: usize, q: usize) -> usize {
    (p + q - 1) / q
}

fn pad(p: usize, q: usize) -> usize {
    ceil_div(p, q) * q
}

fn quantize_q8_1(
    src: &CudaView<f32>,
    dst: &mut CudaSlice<u8>,
    elem_count: usize,
    dev: &CudaDevice,
) -> Result<()> {
    use cudarc::driver::LaunchAsync;

    let kx = elem_count;
    let kx_padded = pad(kx, MATRIX_ROW_PADDING);
    let num_blocks = ceil_div(kx_padded, CUDA_QUANTIZE_BLOCK_SIZE);
    let func = dev.get_or_load_func("quantize_q8_1", candle_kernels::QUANTIZED)?;
    let cfg = cudarc::driver::LaunchConfig {
        grid_dim: (num_blocks as u32, 1, 1),
        block_dim: (CUDA_QUANTIZE_BLOCK_SIZE as u32, 1, 1),
        shared_mem_bytes: 0,
    };
    let params = (src, dst, kx as i32, kx_padded as i32);
    unsafe { func.launch(cfg, params) }.w()?;
    Ok(())
}

fn dequantize(
    data: &CudaSlice<u8>,
    dtype: GgmlDType,
    elem_count: usize,
    dev: &CudaDevice,
) -> Result<CudaStorage> {
    use cudarc::driver::LaunchAsync;

    let nb = (elem_count + 255) / 256;
    let (kernel_name, is_k, block_dim, num_blocks) = match dtype {
        GgmlDType::Q4_0 => ("dequantize_block_q4_0", false, 32, nb),
        GgmlDType::Q4_1 => ("dequantize_block_q4_1", false, 32, nb),
        GgmlDType::Q5_0 => (
            "dequantize_block_q5_0",
            false,
            CUDA_DEQUANTIZE_BLOCK_SIZE,
            ceil_div(elem_count, 2 * CUDA_DEQUANTIZE_BLOCK_SIZE),
        ),
        GgmlDType::Q5_1 => (
            "dequantize_block_q5_1",
            false,
            CUDA_DEQUANTIZE_BLOCK_SIZE,
            ceil_div(elem_count, 2 * CUDA_DEQUANTIZE_BLOCK_SIZE),
        ),
        GgmlDType::Q8_0 => ("dequantize_block_q8_0", false, 32, nb),
        GgmlDType::Q2K => ("dequantize_block_q2_K", true, 64, nb),
        GgmlDType::Q3K => ("dequantize_block_q3_K", true, 64, nb),
        GgmlDType::Q4K => ("dequantize_block_q4_K", true, 32, nb),
        GgmlDType::Q5K => ("dequantize_block_q5_K", true, 64, nb),
        GgmlDType::Q6K => ("dequantize_block_q6_K", true, 64, nb),
        GgmlDType::Q8K => ("dequantize_block_q8_K", true, 32, nb),
        _ => crate::bail!("unsupported dtype for dequantize {dtype:?}"),
    };
    let func = dev.get_or_load_func(kernel_name, candle_kernels::QUANTIZED)?;
    let dst = unsafe { dev.alloc::<f32>(elem_count).w()? };
    // See e.g.
    // https://github.com/ggerganov/llama.cpp/blob/cbbd1efa06f8c09f9dff58ff9d9af509cc4c152b/ggml-cuda.cu#L7270
    let cfg = cudarc::driver::LaunchConfig {
        grid_dim: (num_blocks as u32, 1, 1),
        block_dim: (block_dim as u32, 1, 1),
        shared_mem_bytes: 0,
    };

    if is_k {
        let params = (data, &dst);
        unsafe { func.launch(cfg, params) }.w()?;
    } else {
        let nb32 = match dtype {
            GgmlDType::Q5_0 | GgmlDType::Q5_1 => elem_count,
            _ => elem_count / 32,
        };
        let params = (data, &dst, nb32 as i32);
        unsafe { func.launch(cfg, params) }.w()?;
    }
    Ok(CudaStorage::wrap_cuda_slice(dst, dev.clone()))
}

fn dequantize_mul_mat_vec(
    data: &CudaSlice<u8>,
    y: &CudaView<f32>,
    dtype: GgmlDType,
    ncols: usize,
    nrows: usize,
    dev: &CudaDevice,
) -> Result<CudaStorage> {
    use cudarc::driver::LaunchAsync;

    let data_elems = data.len() / dtype.type_size() * dtype.block_size();
    if data_elems < ncols * nrows {
        crate::bail!("unexpected data size {}, ncols {ncols} {nrows}", data_elems)
    }
    if y.len() != ncols {
        crate::bail!("unexpected y size {}, ncols {ncols} {nrows}", y.len())
    }
    let kernel_name = match dtype {
        GgmlDType::Q4_0 => "dequantize_mul_mat_vec_q4_0_cuda",
        GgmlDType::Q4_1 => "dequantize_mul_mat_vec_q4_1_cuda",
        GgmlDType::Q5_0 => "dequantize_mul_mat_vec_q5_0_cuda",
        GgmlDType::Q5_1 => "dequantize_mul_mat_vec_q5_1_cuda",
        GgmlDType::Q8_0 => "dequantize_mul_mat_vec_q8_0_cuda",
        GgmlDType::Q2K => "dequantize_mul_mat_vec_q2_k",
        GgmlDType::Q3K => "dequantize_mul_mat_vec_q3_k",
        GgmlDType::Q4K => "dequantize_mul_mat_vec_q4_k",
        GgmlDType::Q5K => "dequantize_mul_mat_vec_q5_k",
        GgmlDType::Q6K => "dequantize_mul_mat_vec_q6_k",
        _ => crate::bail!("unsupported dtype for quantized matmul {dtype:?}"),
    };
    let func = dev.get_or_load_func(kernel_name, candle_kernels::QUANTIZED)?;
    let dst = unsafe { dev.alloc::<f32>(nrows).w()? };
    let block_num_y = ceil_div(nrows, GGML_CUDA_MMV_Y);
    let cfg = cudarc::driver::LaunchConfig {
        grid_dim: (block_num_y as u32, 1, 1),
        block_dim: (WARP_SIZE as u32, GGML_CUDA_MMV_Y as u32, 1),
        shared_mem_bytes: 0,
    };

    let params = (data, y, &dst, ncols as i32, nrows as i32);
    unsafe { func.launch(cfg, params) }.w()?;
    Ok(CudaStorage::wrap_cuda_slice(dst, dev.clone()))
}

fn mul_mat_vec_via_q8_1(
    data: &CudaSlice<u8>,
    y: &CudaView<f32>,
    dtype: GgmlDType,
    ncols: usize,
    nrows: usize,
    dev: &CudaDevice,
) -> Result<CudaStorage> {
    use cudarc::driver::LaunchAsync;

    let data_elems = data.len() / dtype.type_size() * dtype.block_size();
    if data_elems < ncols * nrows {
        crate::bail!("unexpected data size {}, ncols {ncols} {nrows}", data_elems)
    }
    if y.len() != ncols {
        crate::bail!("unexpected y size {}, ncols {ncols} {nrows}", y.len())
    }
    // Start by quantizing y
    let ncols_padded = pad(ncols, MATRIX_ROW_PADDING);
    let y_size_in_bytes = ncols_padded * GgmlDType::Q8_1.type_size() / GgmlDType::Q8_1.block_size();
    let mut y_q8_1 = unsafe { dev.alloc::<u8>(y_size_in_bytes).w()? };
    quantize_q8_1(y, &mut y_q8_1, ncols, dev)?;

    let kernel_name = match dtype {
        GgmlDType::Q4_0 => "mul_mat_vec_q4_0_q8_1_cuda",
        GgmlDType::Q4_1 => "mul_mat_vec_q4_1_q8_1_cuda",
        GgmlDType::Q5_0 => "mul_mat_vec_q5_0_q8_1_cuda",
        GgmlDType::Q5_1 => "mul_mat_vec_q5_1_q8_1_cuda",
        GgmlDType::Q8_0 => "mul_mat_vec_q8_0_q8_1_cuda",
        GgmlDType::Q2K => "mul_mat_vec_q2_K_q8_1_cuda",
        GgmlDType::Q3K => "mul_mat_vec_q3_K_q8_1_cuda",
        GgmlDType::Q4K => "mul_mat_vec_q4_K_q8_1_cuda",
        GgmlDType::Q5K => "mul_mat_vec_q5_K_q8_1_cuda",
        GgmlDType::Q6K => "mul_mat_vec_q6_K_q8_1_cuda",
        _ => crate::bail!("unsupported dtype for quantized matmul {dtype:?}"),
    };
    let func = dev.get_or_load_func(kernel_name, candle_kernels::QUANTIZED)?;
    let dst = unsafe { dev.alloc::<f32>(nrows).w()? };
    let cfg = cudarc::driver::LaunchConfig {
        grid_dim: (nrows as u32, 1, 1),
        block_dim: (WARP_SIZE as u32, 4, 1),
        shared_mem_bytes: 0,
    };

    let params = (
        data,
        &y_q8_1,
        &dst,
        /* ncols_x */ ncols as i32,
        /* nrows_x */ nrows as i32,
        /* nrows_y */ ncols as i32,
        /* nrows_dst */ nrows as i32,
    );
    unsafe { func.launch(cfg, params) }.w()?;
    Ok(CudaStorage::wrap_cuda_slice(dst, dev.clone()))
}

impl QCudaStorage {
    pub fn zeros(device: &CudaDevice, el_count: usize, dtype: GgmlDType) -> Result<Self> {
        let size_in_bytes = ceil_div(el_count, dtype.block_size()) * dtype.type_size();
        let data = device.alloc_zeros::<u8>(size_in_bytes).w()?;
        Ok(QCudaStorage {
            data,
            device: device.clone(),
            dtype,
        })
    }

    pub fn dtype(&self) -> GgmlDType {
        self.dtype
    }

    pub fn device(&self) -> &CudaDevice {
        &self.device
    }

    pub fn dequantize(&self, elem_count: usize) -> Result<CudaStorage> {
        fn deq<T: GgmlType>(buffer: &[u8], n: usize, dst: &mut [f32]) -> Result<()> {
            let slice = unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const T, n) };
            let vec = slice.to_vec();
            T::to_float(&vec, dst)
        }

        let fast_kernel = matches!(
            self.dtype,
            GgmlDType::Q4_0
                | GgmlDType::Q4_1
                | GgmlDType::Q5_0
                | GgmlDType::Q5_1
                | GgmlDType::Q8_0
                | GgmlDType::Q2K
                | GgmlDType::Q3K
                | GgmlDType::Q4K
                | GgmlDType::Q5K
                | GgmlDType::Q6K
                | GgmlDType::Q8K
        );
        if fast_kernel {
            return dequantize(&self.data, self.dtype, elem_count, self.device());
        }
        // Run the dequantization on cpu.

        let buffer = self.device.dtoh_sync_copy(&self.data).w()?;
        let mut out = vec![0.0; elem_count];
        let block_len = elem_count / self.dtype.block_size();
        match self.dtype {
            GgmlDType::F32 => deq::<f32>(&buffer, block_len, &mut out)?,
            GgmlDType::F16 => deq::<half::f16>(&buffer, block_len, &mut out)?,
            GgmlDType::Q4_0 => deq::<crate::quantized::BlockQ4_0>(&buffer, block_len, &mut out)?,
            GgmlDType::Q4_1 => deq::<crate::quantized::BlockQ4_1>(&buffer, block_len, &mut out)?,
            GgmlDType::Q5_0 => deq::<crate::quantized::BlockQ5_0>(&buffer, block_len, &mut out)?,
            GgmlDType::Q5_1 => deq::<crate::quantized::BlockQ5_1>(&buffer, block_len, &mut out)?,
            GgmlDType::Q8_0 => deq::<crate::quantized::BlockQ8_0>(&buffer, block_len, &mut out)?,
            GgmlDType::Q8_1 => deq::<crate::quantized::BlockQ8_1>(&buffer, block_len, &mut out)?,
            GgmlDType::Q2K => deq::<crate::quantized::BlockQ2K>(&buffer, block_len, &mut out)?,
            GgmlDType::Q3K => deq::<crate::quantized::BlockQ3K>(&buffer, block_len, &mut out)?,
            GgmlDType::Q4K => deq::<crate::quantized::BlockQ4K>(&buffer, block_len, &mut out)?,
            GgmlDType::Q5K => deq::<crate::quantized::BlockQ5K>(&buffer, block_len, &mut out)?,
            GgmlDType::Q6K => deq::<crate::quantized::BlockQ6K>(&buffer, block_len, &mut out)?,
            GgmlDType::Q8K => deq::<crate::quantized::BlockQ8K>(&buffer, block_len, &mut out)?,
        }

        self.device
            .storage_from_cpu_storage(&crate::CpuStorage::F32(out))
    }

    pub fn quantize(&mut self, src: &CudaStorage) -> Result<()> {
        // Run the quantization on cpu.
        let src = match &src.slice {
            crate::cuda_backend::CudaStorageSlice::F32(data) => {
                self.device.dtoh_sync_copy(data).w()?
            }
            _ => crate::bail!("only f32 can be quantized"),
        };
        let src_len = src.len();
        let src = crate::Storage::Cpu(crate::CpuStorage::F32(src));
        let mut qcpu_storage = crate::Device::Cpu.qzeros(src_len, self.dtype)?;
        qcpu_storage.quantize(&src)?;
        let data = qcpu_storage.data()?;
        let data = self.device.htod_sync_copy(data.as_ref()).w()?;
        self.data = data;
        Ok(())
    }

    pub fn storage_size_in_bytes(&self) -> usize {
        self.data.len()
    }

    pub fn fwd(
        &self,
        self_shape: &crate::Shape,
        storage: &CudaStorage,
        layout: &crate::Layout,
    ) -> Result<(CudaStorage, crate::Shape)> {
        if matches!(layout.shape().dims(), [1, 1, _] | [1, _]) {
            self.dequantize_matmul_vec(self_shape, storage, layout)
        } else {
            self.dequantize_matmul(self_shape, storage, layout)
        }
    }
}

impl QCudaStorage {
    fn dequantize_matmul_vec(
        &self,
        self_shape: &crate::Shape,
        rhs: &CudaStorage,
        rhs_l: &crate::Layout,
    ) -> Result<(CudaStorage, crate::Shape)> {
        let (nrows, ncols) = self_shape.dims2()?;
        let rhs = rhs.as_cuda_slice::<f32>()?;
        let rhs = match rhs_l.contiguous_offsets() {
            Some((o1, o2)) => rhs.slice(o1..o2),
            None => Err(crate::Error::RequiresContiguous { op: "dmmv" }.bt())?,
        };
        let (with_batch, k) = match rhs_l.shape().dims() {
            [1, 1, k] => (true, k),
            [1, k] => (false, k),
            _ => crate::bail!("unexpected rhs shape in dmmv {:?}", rhs_l.shape()),
        };
        if ncols != *k {
            crate::bail!("mismatch on matmul dim {self_shape:?} {:?}", rhs_l.shape())
        }

        let out = if FORCE_DMMV.load(std::sync::atomic::Ordering::Relaxed) {
            dequantize_mul_mat_vec(&self.data, &rhs, self.dtype, ncols, nrows, self.device())?
        } else {
            mul_mat_vec_via_q8_1(&self.data, &rhs, self.dtype, ncols, nrows, self.device())?
        };
        let out_shape = if with_batch {
            vec![1, 1, nrows]
        } else {
            vec![1, nrows]
        };
        Ok((out, out_shape.into()))
    }

    fn dequantize_matmul(
        &self,
        self_shape: &crate::Shape,
        storage: &CudaStorage,
        layout: &crate::Layout,
    ) -> Result<(CudaStorage, crate::Shape)> {
        use crate::backend::BackendStorage;
        let (n, k) = self_shape.dims2()?;
        let (b, m, k2) = match layout.shape().dims() {
            &[b, m, k2] => (b, m, k2),
            &[m, k2] => (1, m, k2),
            s => crate::bail!("unexpected shape for input {s:?}"),
        };
        if k2 != k {
            crate::bail!("mismatch on matmul dim {self_shape:?} {:?}", layout.shape())
        }

        let data_f32 = self.dequantize(n * k)?;
        let rhs_l = crate::Layout::new((k, n).into(), vec![1, k], 0).broadcast_as((b, k, n))?;
        let out = storage.matmul(&data_f32, (b, m, n, k), layout, &rhs_l)?;
        let mut out_shape = layout.shape().dims().to_vec();
        out_shape.pop();
        out_shape.push(n);
        Ok((out, out_shape.into()))
    }
}

pub fn load_quantized<T: super::GgmlType + Send + Sync + 'static>(
    device: &CudaDevice,
    data: &[T],
) -> Result<super::QStorage> {
    let data = unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, core::mem::size_of_val(data))
    };
    let data = device.htod_sync_copy(data).w()?;
    Ok(QStorage::Cuda(QCudaStorage {
        data,
        device: device.clone(),
        dtype: T::DTYPE,
    }))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn cuda_quantize_q8_1() -> Result<()> {
        let dev = CudaDevice::new(0)?;
        let el = 256;
        let el_padded = pad(el, MATRIX_ROW_PADDING);
        let y_size_in_bytes =
            el_padded * GgmlDType::Q8_1.type_size() / GgmlDType::Q8_1.block_size();
        let mut y_q8_1 = unsafe { dev.alloc::<u8>(y_size_in_bytes).w()? };
        let vs: Vec<f32> = (0..el).map(|v| v as f32).collect();
        let y = dev.htod_sync_copy(&vs).w()?;
        quantize_q8_1(&y.slice(..), &mut y_q8_1, el, &dev)?;
        Ok(())
    }

    #[test]
    fn cuda_mmv_q8_1() -> Result<()> {
        let dev = CudaDevice::new(0)?;
        let ncols = 256;
        let vs: Vec<f32> = (0..ncols).map(|v| v as f32).collect();
        let y = dev.htod_sync_copy(&vs).w()?;
        let mut xs = QCudaStorage::zeros(&dev, ncols, GgmlDType::Q4_0)?;
        xs.quantize(&CudaStorage::wrap_cuda_slice(y.clone(), dev.clone()))?;
        let cuda_storage = mul_mat_vec_via_q8_1(
            &xs.data,
            &y.slice(..),
            /* dtype */ GgmlDType::Q4_0,
            /* ncols */ ncols,
            /* nrows */ 1,
            &dev,
        )?;
        let vs = cuda_storage.as_cuda_slice::<f32>()?;
        let vs = dev.dtoh_sync_copy(&vs.slice(..)).unwrap();
        assert_eq!(vs.len(), 1);
        // for n = 255, n.(n+1).(2n+1) / 6 = 5559680
        // Q8 means 1/256 precision.
        assert_eq!(vs[0], 5561664.5);

        let cuda_storage = dequantize_mul_mat_vec(
            &xs.data,
            &y.slice(..),
            /* dtype */ GgmlDType::Q4_0,
            /* ncols */ ncols,
            /* nrows */ 1,
            &dev,
        )?;
        let vs = cuda_storage.as_cuda_slice::<f32>()?;
        let vs = dev.dtoh_sync_copy(&vs.slice(..)).unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0], 5561851.0);
        Ok(())
    }
}
