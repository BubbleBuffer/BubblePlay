use std::ffi::c_void;
use std::slice;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude, D3D_SRV_DIMENSION_TEXTURE2D};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11Device3, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BUFFER_UAV,
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SHADER_RESOURCE_VIEW_DESC1,
    D3D11_SHADER_RESOURCE_VIEW_DESC1_0, D3D11_TEX2D_SRV1, D3D11_TEXTURE2D_DESC,
    D3D11_UAV_DIMENSION_BUFFER, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

const CTB_SIZE: u32 = 32;

const RECONSTRUCTION_ERROR_SHADER: &str = r#"
Texture2D<float4> InputFrame : register(t0);
Texture2D<float> ReconstructedLuma : register(t1);
RWStructuredBuffer<uint> CtbErrors : register(u0);

groupshared uint PartialErrors[64];

[numthreads(8, 8, 1)]
void main(
    uint3 group_id : SV_GroupID,
    uint3 group_thread_id : SV_GroupThreadID,
    uint group_index : SV_GroupIndex
) {
    uint width;
    uint height;
    InputFrame.GetDimensions(width, height);
    uint2 origin = group_id.xy * 32 + group_thread_id.xy * 4;
    uint error_sum = 0;
    [unroll]
    for (uint y = 0; y < 4; ++y) {
        [unroll]
        for (uint x = 0; x < 4; ++x) {
            uint2 position = origin + uint2(x, y);
            if (position.x >= width || position.y >= height) {
                continue;
            }
            uint3 input = (uint3)round(saturate(InputFrame.Load(int3(position, 0)).rgb) * 255.0);
            uint input_luma = (input.x * 19 + input.y * 183 + input.z * 54) >> 8;
            uint recon_limited = (uint)round(saturate(ReconstructedLuma.Load(int3(position, 0))) * 255.0);
            uint recon_luma = recon_limited <= 16
                ? 0
                : min(255, ((recon_limited - 16) * 255 + 109) / 219);
            error_sum += (uint)abs((int)input_luma - (int)recon_luma);
        }
    }
    PartialErrors[group_index] = error_sum;
    GroupMemoryBarrierWithGroupSync();
    [unroll]
    for (uint stride = 32; stride > 0; stride >>= 1) {
        if (group_index < stride) {
            PartialErrors[group_index] += PartialErrors[group_index + stride];
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (group_index == 0) {
        uint columns = (width + 31) / 32;
        CtbErrors[group_id.y * columns + group_id.x] = PartialErrors[0];
    }
}
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconstructionErrorFrame {
    pub sequence: u32,
    pub errors: Vec<u32>,
    pub age_frames: u32,
    pub readback_us: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconstructionFeedbackStep {
    pub completed: Option<ReconstructionErrorFrame>,
    pub submitted: bool,
    pub submit_us: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconstructionFeedbackSubmission {
    pub submitted: bool,
    pub submit_us: u32,
}

pub const fn is_exact_previous_feedback(
    current_sequence: u32,
    completed_sequence: u32,
    age_frames: u32,
) -> bool {
    age_frames == 1 && completed_sequence == current_sequence.wrapping_sub(1)
}

#[derive(Debug, Clone, Copy)]
struct ReadbackTicket {
    sequence: u32,
    order: u64,
}

pub struct ReconstructionFeedbackAnalyzer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    shader: ID3D11ComputeShader,
    input_copy: ID3D11Texture2D,
    input_view: ID3D11ShaderResourceView,
    reconstruction_textures: [ID3D11Texture2D; 2],
    reconstruction_views: [ID3D11ShaderResourceView; 2],
    error_buffer: ID3D11Buffer,
    error_view: ID3D11UnorderedAccessView,
    readback_buffers: [ID3D11Buffer; 2],
    readbacks: [Option<ReadbackTicket>; 2],
    next_readback: usize,
    submit_order: u64,
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
}

impl ReconstructionFeedbackAnalyzer {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        ensure!(
            width > 0 && height > 0,
            "reconstruction feedback dimensions must be nonzero"
        );
        ensure!(
            width % 2 == 0 && height % 2 == 0,
            "NV12 reconstruction dimensions must be even"
        );
        let columns = width.div_ceil(CTB_SIZE);
        let rows = height.div_ceil(CTB_SIZE);
        let total_ctbs = columns
            .checked_mul(rows)
            .context("reconstruction feedback CTB count overflow")?;
        ensure!(
            total_ctbs <= 8_160,
            "reconstruction feedback CTB grid exceeds lab bound"
        );
        let context = unsafe { device.GetImmediateContext() }
            .context("reconstruction feedback GetImmediateContext")?;
        let shader = compile_shader(device)?;
        let input_copy = create_texture(
            device,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
        )?;
        let input_view = create_default_view(device, &input_copy)?;
        let first_recon = create_texture(
            device,
            width,
            height,
            DXGI_FORMAT_NV12,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
        )?;
        let second_recon = create_texture(
            device,
            width,
            height,
            DXGI_FORMAT_NV12,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
        )?;
        let first_view = create_nv12_luma_view(device, &first_recon)?;
        let second_view = create_nv12_luma_view(device, &second_recon)?;
        let error_buffer = create_error_buffer(device, total_ctbs)?;
        let error_view = create_error_view(device, &error_buffer, total_ctbs)?;
        let first_readback = create_readback_buffer(device, total_ctbs)?;
        let second_readback = create_readback_buffer(device, total_ctbs)?;
        Ok(Self {
            device: device.clone(),
            context,
            shader,
            input_copy,
            input_view,
            reconstruction_textures: [first_recon, second_recon],
            reconstruction_views: [first_view, second_view],
            error_buffer,
            error_view,
            readback_buffers: [first_readback, second_readback],
            readbacks: [None, None],
            next_readback: 0,
            submit_order: 0,
            width,
            height,
            columns,
            rows,
        })
    }

    pub fn is_for(&self, device: &ID3D11Device, width: u32, height: u32) -> bool {
        self.device.as_raw() == device.as_raw() && self.width == width && self.height == height
    }

    pub fn reconstruction_texture(&self, sequence: u32) -> &ID3D11Texture2D {
        &self.reconstruction_textures[sequence as usize % self.reconstruction_textures.len()]
    }

    pub fn reconstruction_texture_raw(&self, sequence: u32) -> *mut c_void {
        self.reconstruction_texture(sequence).as_raw()
    }

    pub fn reset(&mut self) {
        self.readbacks = [None, None];
        self.next_readback = 0;
        self.submit_order = 0;
    }

    pub fn submit_and_collect(
        &mut self,
        source: &ID3D11Texture2D,
        sequence: u32,
    ) -> Result<ReconstructionFeedbackStep> {
        let completed = self.collect_ready(sequence)?;
        let submission = self.submit(source, sequence)?;
        Ok(ReconstructionFeedbackStep {
            completed,
            submitted: submission.submitted,
            submit_us: submission.submit_us,
        })
    }

    pub fn collect_ready(
        &mut self,
        current_sequence: u32,
    ) -> Result<Option<ReconstructionErrorFrame>> {
        self.collect_oldest(current_sequence, true)
    }

    pub fn submit(
        &mut self,
        source: &ID3D11Texture2D,
        sequence: u32,
    ) -> Result<ReconstructionFeedbackSubmission> {
        self.validate_source(source)?;
        let Some(readback_slot) = self.free_readback() else {
            return Ok(ReconstructionFeedbackSubmission {
                submitted: false,
                submit_us: 0,
            });
        };
        let started = Instant::now();
        let recon_index = sequence as usize % self.reconstruction_views.len();
        unsafe {
            self.context.CopyResource(&self.input_copy, source);
        }
        let shader_resources = [
            Some(self.input_view.clone()),
            Some(self.reconstruction_views[recon_index].clone()),
        ];
        let unordered_views = [Some(self.error_view.clone())];
        unsafe {
            self.context.CSSetShader(&self.shader, None);
            self.context
                .CSSetShaderResources(0, Some(&shader_resources));
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(unordered_views.as_ptr()), None);
            self.context.Dispatch(self.columns, self.rows, 1);
            self.context.CSSetShaderResources(0, Some(&[None, None]));
            self.context.CSSetUnorderedAccessViews(
                0,
                1,
                Some([None::<ID3D11UnorderedAccessView>].as_ptr()),
                None,
            );
            self.context.CSSetShader(None::<&ID3D11ComputeShader>, None);
            self.context
                .CopyResource(&self.readback_buffers[readback_slot], &self.error_buffer);
            // DO_NOT_WAIT collection on the following frame cannot observe
            // commands that the D3D11 driver still holds in its CPU-side
            // command buffer. Submit the reducer now without waiting for it,
            // so frame N's compact CTB result can be collected for frame N+1.
            self.context.Flush();
        }
        self.submit_order = self.submit_order.wrapping_add(1).max(1);
        self.readbacks[readback_slot] = Some(ReadbackTicket {
            sequence,
            order: self.submit_order,
        });
        self.next_readback = (readback_slot + 1) % self.readbacks.len();
        Ok(ReconstructionFeedbackSubmission {
            submitted: true,
            submit_us: elapsed_us(started),
        })
    }

    pub fn collect_blocking(
        &mut self,
        current_sequence: u32,
    ) -> Result<Option<ReconstructionErrorFrame>> {
        self.collect_oldest(current_sequence, false)
    }

    fn collect_oldest(
        &mut self,
        current_sequence: u32,
        do_not_wait: bool,
    ) -> Result<Option<ReconstructionErrorFrame>> {
        let Some(slot) = self.oldest_readback() else {
            return Ok(None);
        };
        let ticket = self.readbacks[slot].expect("oldest feedback readback lost its ticket");
        let started = Instant::now();
        let flags = if do_not_wait {
            D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32
        } else {
            0
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let result = unsafe {
            self.context.Map(
                &self.readback_buffers[slot],
                0,
                D3D11_MAP_READ,
                flags,
                Some(&mut mapped),
            )
        };
        if let Err(error) = result {
            if do_not_wait && error.code() == DXGI_ERROR_WAS_STILL_DRAWING {
                return Ok(None);
            }
            return Err(error).context("reconstruction feedback readback");
        }
        let total_ctbs = (self.columns * self.rows) as usize;
        let errors =
            unsafe { slice::from_raw_parts(mapped.pData.cast::<u32>(), total_ctbs).to_vec() };
        unsafe { self.context.Unmap(&self.readback_buffers[slot], 0) };
        self.readbacks[slot] = None;
        Ok(Some(ReconstructionErrorFrame {
            sequence: ticket.sequence,
            errors,
            age_frames: current_sequence.wrapping_sub(ticket.sequence),
            readback_us: elapsed_us(started),
        }))
    }

    fn oldest_readback(&self) -> Option<usize> {
        self.readbacks
            .iter()
            .enumerate()
            .filter_map(|(index, ticket)| ticket.map(|ticket| (index, ticket.order)))
            .min_by_key(|(_, order)| *order)
            .map(|(index, _)| index)
    }

    fn free_readback(&self) -> Option<usize> {
        (0..self.readbacks.len())
            .map(|offset| (self.next_readback + offset) % self.readbacks.len())
            .find(|slot| self.readbacks[*slot].is_none())
    }

    fn validate_source(&self, source: &ID3D11Texture2D) -> Result<()> {
        let source_device =
            unsafe { source.GetDevice() }.context("reconstruction feedback source GetDevice")?;
        ensure!(
            source_device.as_raw() == self.device.as_raw(),
            "reconstruction feedback source belongs to a different D3D11 device"
        );
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        ensure!(
            desc.Width == self.width
                && desc.Height == self.height
                && desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM
                && desc.SampleDesc.Count == 1,
            "reconstruction feedback source must be single-sample BGRA at {}x{}",
            self.width,
            self.height
        );
        Ok(())
    }
}

fn elapsed_us(started: Instant) -> u32 {
    started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32
}

fn compile_shader(device: &ID3D11Device) -> Result<ID3D11ComputeShader> {
    let mut bytecode = None;
    let mut errors = None;
    let compiled = unsafe {
        D3DCompile(
            RECONSTRUCTION_ERROR_SHADER.as_ptr().cast::<c_void>(),
            RECONSTRUCTION_ERROR_SHADER.len(),
            PCSTR(b"bubbleplay-reconstruction-feedback\0".as_ptr()),
            None,
            None::<&ID3DInclude>,
            PCSTR(b"main\0".as_ptr()),
            PCSTR(b"cs_5_0\0".as_ptr()),
            0,
            0,
            &mut bytecode,
            Some(&mut errors),
        )
    };
    if let Err(error) = compiled {
        let detail = errors
            .as_ref()
            .map(blob_text)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| error.to_string());
        bail!("compile reconstruction feedback compute shader: {detail}");
    }
    let bytecode = bytecode.context("reconstruction feedback compiler returned no bytecode")?;
    let bytes = unsafe {
        slice::from_raw_parts(
            bytecode.GetBufferPointer().cast::<u8>(),
            bytecode.GetBufferSize(),
        )
    };
    let mut shader = None;
    unsafe { device.CreateComputeShader(bytes, None, Some(&mut shader)) }
        .context("CreateComputeShader(reconstruction feedback)")?;
    shader.context("CreateComputeShader(reconstruction feedback) returned no object")
}

fn blob_text(blob: &ID3DBlob) -> String {
    let bytes = unsafe {
        slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim()
        .to_string()
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bind_flags: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D(reconstruction feedback)")?;
    texture.context("CreateTexture2D(reconstruction feedback) returned no object")
}

fn create_default_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView> {
    let mut view = None;
    unsafe { device.CreateShaderResourceView(texture, None, Some(&mut view)) }
        .context("CreateShaderResourceView(reconstruction feedback input)")?;
    view.context("CreateShaderResourceView(reconstruction feedback input) returned no object")
}

fn create_nv12_luma_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView> {
    let device3: ID3D11Device3 = device
        .cast()
        .context("D3D11 device does not expose plane-slice shader resource views")?;
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC1 {
        Format: DXGI_FORMAT_R8_UNORM,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC1_0 {
            Texture2D: D3D11_TEX2D_SRV1 {
                MostDetailedMip: 0,
                MipLevels: 1,
                PlaneSlice: 0,
            },
        },
    };
    let mut view = None;
    unsafe { device3.CreateShaderResourceView1(texture, Some(&desc), Some(&mut view)) }
        .context("CreateShaderResourceView1(reconstruction luma plane)")?;
    view.context("CreateShaderResourceView1(reconstruction luma plane) returned no object")?
        .cast()
        .context("cast reconstruction luma view")
}

fn create_error_buffer(device: &ID3D11Device, total_ctbs: u32) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: total_ctbs * std::mem::size_of::<u32>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: std::mem::size_of::<u32>() as u32,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .context("CreateBuffer(reconstruction errors)")?;
    buffer.context("CreateBuffer(reconstruction errors) returned no object")
}

fn create_error_view(
    device: &ID3D11Device,
    buffer: &ID3D11Buffer,
    total_ctbs: u32,
) -> Result<ID3D11UnorderedAccessView> {
    let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_UNKNOWN,
        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_UAV {
                FirstElement: 0,
                NumElements: total_ctbs,
                Flags: 0,
            },
        },
    };
    let mut view = None;
    unsafe { device.CreateUnorderedAccessView(buffer, Some(&desc), Some(&mut view)) }
        .context("CreateUnorderedAccessView(reconstruction errors)")?;
    view.context("CreateUnorderedAccessView(reconstruction errors) returned no object")
}

fn create_readback_buffer(device: &ID3D11Device, total_ctbs: u32) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: total_ctbs * std::mem::size_of::<u32>() as u32,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .context("CreateBuffer(reconstruction feedback readback)")?;
    buffer.context("CreateBuffer(reconstruction feedback readback) returned no object")
}

#[cfg(test)]
mod tests {
    use super::is_exact_previous_feedback;

    #[test]
    fn only_exact_n_minus_one_feedback_is_eligible_including_sequence_wrap() {
        assert!(is_exact_previous_feedback(42, 41, 1));
        assert!(is_exact_previous_feedback(0, u32::MAX, 1));
        assert!(!is_exact_previous_feedback(42, 40, 2));
        assert!(!is_exact_previous_feedback(42, 41, 2));
        assert!(!is_exact_previous_feedback(42, 42, 0));
    }
}
