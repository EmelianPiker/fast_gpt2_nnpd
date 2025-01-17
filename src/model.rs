use crate::ops::{add, addmm, attention, gelu, matmul_t, mul, normalize, select};
use crate::tensor::{OwnedTensor, PastKeyValue, PastKeyValues, Tensor, TensorMut, ViewTensor};
use safetensors::tensor::{SafeTensors, TensorView};

#[derive(Clone)]
pub struct Mlp<'a> {
    pub c_fc: Linear<'a>,
    pub c_proj: Linear<'a>,
}

impl<'a> Mlp<'a> {
    fn from_tensors(index: usize, tensors: &'a SafeTensors<'a>) -> Self {
        let c_fc = Linear::from(
            tensors
                .tensor(&format!("h.{index}.mlp.c_fc.weight"))
                .unwrap(),
            tensors.tensor(&format!("h.{index}.mlp.c_fc.bias")).unwrap(),
        );
        let c_proj = Linear::from(
            tensors
                .tensor(&format!("h.{index}.mlp.c_proj.weight"))
                .unwrap(),
            tensors
                .tensor(&format!("h.{index}.mlp.c_proj.bias"))
                .unwrap(),
        );
        Self { c_fc, c_proj }
    }

    pub fn forward(&self, tensor: &mut OwnedTensor) {
        self.c_fc.forward(tensor);
        gelu(tensor);
        self.c_proj.forward(tensor);
    }

    async fn special_forward(&self, tensor: &mut OwnedTensor) {
        println!(
            "SHAPES {:#?}",
            (
                &self.c_fc.weight.shape,
                &self.c_fc.bias.shape,
                &self.c_proj.weight.shape,
                &self.c_proj.bias.shape
            )
        );
        let mut data: Vec<f32> = vec![];
        if tensor.shape[0] == 1 && tensor.shape[1] == 768 {
            use jsonrpsee_core::client::ClientT;
            use jsonrpsee_http_client::{HeaderMap, HeaderValue, HttpClientBuilder};
            let client = HttpClientBuilder::default()
                .build("http://127.0.0.1:9221")
                .unwrap();
            let mut input: Vec<u8> = vec![];
            for i in 0..768 {
                let mut part = [0_u8; 4];
                part = unsafe { std::mem::transmute(tensor.data[i]) };
                input.push(part[0]);
                input.push(part[1]);
                input.push(part[2]);
                input.push(part[3]);
            }
            let out = client
                .request::<String, _>("nn_compute_out", vec![hex::encode(input)])
                .await;
            let dec = hex::decode(&out.as_ref().unwrap().as_bytes()[2..]).unwrap();
            let len = dec.len();
            for i in 0..768 {
                let mut part = [0_u8; 4];
                if (i * 4 + 3) < len {
                    part[0] = dec[i * 4 + 0];
                    part[1] = dec[i * 4 + 1];
                    part[2] = dec[i * 4 + 2];
                    part[3] = dec[i * 4 + 3];
                }
                data.push(unsafe { std::mem::transmute(part) });
            }
        }

        self.c_fc.forward(tensor);
        gelu(tensor);
        self.c_proj.forward(tensor);

        if tensor.shape[0] == 1 && tensor.shape[1] == 768 {
            let mut diff = vec![];
            for i in 0..768 {
                if tensor.data[i] != data[i] {
                    diff.push((tensor.data[i], data[i]));
                }
            }
            if diff.len() > 0 {
                panic!("{:#?}", diff);
            }
            tensor.data = data;
        }
    }
}

#[derive(Clone)]
pub struct Attention<'a> {
    c_attn: Linear<'a>,
    c_proj: Linear<'a>,
    num_heads: usize,
}

impl<'a> Attention<'a> {
    fn from_tensors(index: usize, tensors: &'a SafeTensors<'a>, num_heads: usize) -> Self {
        let c_attn = Linear::from(
            tensors
                .tensor(&format!("h.{index}.attn.c_attn.weight"))
                .unwrap(),
            tensors
                .tensor(&format!("h.{index}.attn.c_attn.bias"))
                .unwrap(),
        );
        let c_proj = Linear::from(
            tensors
                .tensor(&format!("h.{index}.attn.c_proj.weight"))
                .unwrap(),
            tensors
                .tensor(&format!("h.{index}.attn.c_proj.bias"))
                .unwrap(),
        );
        Self {
            c_attn,
            c_proj,
            num_heads,
        }
    }

    pub fn forward(&self, hidden_states: &mut OwnedTensor, past: &mut PastKeyValue) {
        assert_eq!(hidden_states.shape().len(), 2);
        let sequence_length = hidden_states.shape()[0];
        let hidden_dim = hidden_states.shape()[1];
        self.c_attn.forward(hidden_states);
        let qkv = hidden_states;
        let num_heads = self.num_heads;
        assert_eq!(hidden_dim % num_heads, 0);
        let head_dim = hidden_dim / num_heads;
        let past_sequence_length = past.key.shape()[1];
        let mut qk = OwnedTensor::zeros(vec![
            num_heads,
            sequence_length,
            past_sequence_length + sequence_length,
        ]);
        let mut qv = OwnedTensor::zeros(vec![num_heads, sequence_length, head_dim]);
        let mut max = vec![0.0; (past_sequence_length + sequence_length) * num_heads];
        attention(qkv, &mut qk, &mut max, past, &mut qv);
        self.c_proj.forward(&mut qv);
        *qkv = qv;
    }
}

#[derive(Clone)]
pub struct Gpt2Layer<'a> {
    ln_1: LayerNorm<'a>,
    ln_2: LayerNorm<'a>,
    pub mlp: Mlp<'a>,
    attention: Attention<'a>,
}

impl<'a> Gpt2Layer<'a> {
    fn from_tensors(index: usize, tensors: &'a SafeTensors<'a>, num_heads: usize) -> Self {
        let ln_1 = LayerNorm::from(
            tensors.tensor(&format!("h.{index}.ln_1.weight")).unwrap(),
            tensors.tensor(&format!("h.{index}.ln_1.bias")).unwrap(),
        );
        let ln_2 = LayerNorm::from(
            tensors.tensor(&format!("h.{index}.ln_2.weight")).unwrap(),
            tensors.tensor(&format!("h.{index}.ln_2.bias")).unwrap(),
        );
        let mlp = Mlp::from_tensors(index, tensors);
        let attention = Attention::from_tensors(index, tensors, num_heads);
        Self {
            ln_1,
            ln_2,
            mlp,
            attention,
        }
    }

    fn forward(&self, tensor: &mut OwnedTensor, past_key_value: &mut PastKeyValue) {
        let residual = tensor.clone();
        self.ln_1.forward(tensor);
        self.attention.forward(tensor, past_key_value);
        add(&residual, tensor);
        let residual = tensor.clone();
        self.ln_2.forward(tensor);
        self.mlp.forward(tensor);
        add(&residual, tensor);
    }

    async fn special_forward(&self, tensor: &mut OwnedTensor, past_key_value: &mut PastKeyValue) {
        let residual = tensor.clone();
        self.ln_1.forward(tensor);
        self.attention.forward(tensor, past_key_value);
        add(&residual, tensor);
        let residual = tensor.clone();
        self.ln_2.forward(tensor);
        self.mlp.special_forward(tensor).await;
        add(&residual, tensor);
    }
}

#[derive(Clone)]
pub struct Gpt2Model<'a> {
    pub layers: Vec<Gpt2Layer<'a>>,
}

impl<'a> Gpt2Model<'a> {
    fn from_tensors(tensors: &'a SafeTensors<'a>, num_heads: usize) -> Self {
        let layers: Vec<_> = (0..12)
            .map(|i| Gpt2Layer::from_tensors(i, tensors, num_heads))
            .collect();
        Self { layers }
    }

    async fn forward(&self, tensor: &mut OwnedTensor, past_key_values: &mut PastKeyValues) {
        let mut first = true;
        for (layer, past_key_value) in self.layers.iter().zip(past_key_values.iter_mut()) {
            if first {
                layer.special_forward(tensor, past_key_value).await;
            } else {
                layer.forward(tensor, past_key_value);
            }
            first = false;
        }
    }
}

#[derive(Clone)]
pub struct Linear<'a> {
    pub weight: ViewTensor<'a>,
    pub bias: ViewTensor<'a>,
}

impl<'a> std::fmt::Debug for Linear<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Linear")
            .field("shape", &self.weight.shape())
            .finish()
    }
}

impl<'a> Linear<'a> {
    pub fn new(weight: ViewTensor<'a>, bias: ViewTensor<'a>) -> Self {
        Self { weight, bias }
    }

    fn from(weight: TensorView<'a>, bias: TensorView<'a>) -> Self {
        let weight: ViewTensor = weight.into();
        let bias: ViewTensor = bias.into();
        Self::new(weight, bias)
    }

    pub fn forward(&self, tensor: &mut OwnedTensor) {
        assert_eq!(tensor.shape().len(), 2);
        let m = tensor.shape()[0];
        let n = self.weight.shape()[1];
        let mut c = OwnedTensor::new(vec![0.0; n * m], vec![m, n]);
        addmm(tensor, &self.weight, &self.bias, &mut c);
        *tensor = c;
    }
}

#[derive(Clone)]
pub struct UnbiasedLinear<'a> {
    weight: ViewTensor<'a>,
}

impl<'a> std::fmt::Debug for UnbiasedLinear<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnbiasedLinear")
            .field("shape", &self.weight.shape())
            .finish()
    }
}

impl<'a> UnbiasedLinear<'a> {
    fn from(weight: TensorView<'a>) -> Self {
        let weight: ViewTensor = weight.into();
        Self { weight }
    }

    fn forward(&self, tensor: &mut OwnedTensor) {
        let m = tensor.shape()[0];
        let n = self.weight.shape()[0];
        let mut c = OwnedTensor::new(vec![0.0; n * m], vec![m, n]);
        matmul_t(tensor, &self.weight, &mut c);
        *tensor = c;
    }
}

#[derive(Clone)]
pub struct Embedding<'a> {
    weight: ViewTensor<'a>,
}

impl<'a> Embedding<'a> {
    fn from(weight: TensorView<'a>) -> Self {
        let weight: ViewTensor = weight.into();
        Self { weight }
    }

    fn forward(&self, ids: &[u32]) -> OwnedTensor {
        let _vocab_size = self.weight.shape()[0];
        let hidden_dim = self.weight.shape()[1];
        let shape = vec![ids.len(), hidden_dim];
        let data = vec![0.0; ids.len() * hidden_dim];
        let mut tensor = OwnedTensor::new(data, shape);
        select(ids, &self.weight, &mut tensor);
        tensor
    }
}

#[derive(Clone)]
pub struct LayerNorm<'a> {
    weight: ViewTensor<'a>,
    bias: ViewTensor<'a>,
    epsilon: f32,
}

impl<'a> LayerNorm<'a> {
    fn from(weight: TensorView<'a>, bias: TensorView<'a>) -> Self {
        let weight: ViewTensor = weight.into();
        let bias: ViewTensor = bias.into();
        let epsilon = 1e-5;
        Self {
            weight,
            bias,
            epsilon,
        }
    }

    fn forward(&self, tensor: &mut OwnedTensor) {
        let m = tensor.shape()[0];
        let mut mean = vec![0.0; m];
        let mut var = vec![0.0; m];
        normalize(tensor, &mut mean, &mut var, self.epsilon);
        mul(&self.weight, tensor);
        add(&self.bias, tensor);
    }
}

#[derive(Clone)]
pub struct Gpt2<'a> {
    wte: Embedding<'a>,
    wpe: Embedding<'a>,
    pub h: Gpt2Model<'a>,
    ln_f: LayerNorm<'a>,
    lm_head: UnbiasedLinear<'a>,
    num_heads: usize,
}

impl<'a> Gpt2<'a> {
    pub fn from_tensors(tensors: &'a SafeTensors<'a>, num_heads: usize) -> Self {
        let wte = Embedding::from(tensors.tensor("wte.weight").unwrap());
        let wpe = Embedding::from(tensors.tensor("wpe.weight").unwrap());
        let h = Gpt2Model::from_tensors(tensors, num_heads);
        let ln_f = LayerNorm::from(
            tensors.tensor("ln_f.weight").unwrap(),
            tensors.tensor("ln_f.bias").unwrap(),
        );
        let lm_head = UnbiasedLinear::from(tensors.tensor("wte.weight").unwrap());
        Self {
            h,
            ln_f,
            wte,
            wpe,
            lm_head,
            num_heads,
        }
    }
}

impl<'a> Gpt2<'a> {
    pub fn empty_past_key_values(&self) -> PastKeyValues {
        let num_layers = self.h.layers.len();
        let hidden_dim = self.wte.weight.shape()[1];
        let num_heads = self.num_heads;
        assert_eq!(hidden_dim % num_heads, 0);
        let head_dim = hidden_dim / num_heads;
        (0..num_layers)
            .map(|_| PastKeyValue::new(num_heads, 0, head_dim))
            .collect()
    }

    pub async fn forward(&self, ids: &[u32], past: &mut PastKeyValues) -> OwnedTensor {
        let mut tensor = self.wte.forward(ids);
        let past_sequence_length = past[0].key.shape()[1];
        let positions: Vec<u32> = (0..ids.len())
            .map(|i| (i + past_sequence_length) as u32)
            .collect();
        let position_embeddings = self.wpe.forward(&positions[..]);
        add(&position_embeddings, &mut tensor);
        self.h.forward(&mut tensor, past).await;
        self.ln_f.forward(&mut tensor);
        self.lm_head.forward(&mut tensor);
        tensor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tensor::{OwnedTensor, TensorMut, ViewTensor};
    use crate::tests::simplify;
    use memmap2::MmapOptions;

    #[test]
    fn tensor_values() {
        let filename = "model.safetensors";
        let file = std::fs::File::open(filename).unwrap();
        let buffer = unsafe { MmapOptions::new().map(&file).unwrap() };
        let tensors = SafeTensors::deserialize(&buffer).unwrap();
        let tensor: ViewTensor = tensors.tensor("ln_f.weight").unwrap().into();
        let data = tensor.data();
        assert_eq!(
            simplify(&data[..10]),
            // Values obtained through python
            [1.3971, 1.3750, 1.8870, 1.1688, 1.2724, 1.2508, 9.4198, 1.4371, 1.4527, 1.1856]
        );
        assert_eq!(
            simplify(&data[data.len() - 10..]),
            // Values obtained through python
            [1.1758, 1.4514, 1.1525, 1.1731, 4.2194, 1.1660, 1.1625, 1.1034, 1.0980, 1.2070]
        );
    }

    #[test]
    fn embedding() {
        let filename = "model.safetensors";
        let file = std::fs::File::open(filename).unwrap();
        let buffer = unsafe { MmapOptions::new().map(&file).unwrap() };
        let tensors = SafeTensors::deserialize(&buffer).unwrap();
        let tensor = tensors.tensor("wte.weight").unwrap();
        let embedding = Embedding::from(tensor);
        assert_eq!(
            simplify(&embedding.weight.data()[..10]),
            // Values obtained through python
            [
                -0.1101, -0.0393, 0.0331, 0.1338, -0.0485, -0.0789, -0.2398, -0.0895, 0.0253,
                -0.1074
            ]
        );
        let out = embedding.forward(&[1, 256, 50256]);
        let data = out.data();
        assert_eq!(out.shape(), [3, 768]);
        assert_eq!(
            simplify(&data[..10]),
            // Values obtained through python
            [0.0403, -0.0486, 0.0462, -0.0990, 0.0826, 0.0768, -0.2202, -0.0110, 0.0592, 0.0354]
        );
        assert_eq!(
            simplify(&data[data.len() - 10..]),
            // Values obtained through python
            [-0.0499, 0.0689, 0.0123, -0.2156, -0.1742, -0.0373, 0.0930, 0.0070, 0.1552, 0.1207]
        );
    }

    #[test]
    fn layer_norm() {
        let filename = "model.safetensors";
        let file = std::fs::File::open(filename).unwrap();
        let buffer = unsafe { MmapOptions::new().map(&file).unwrap() };
        let tensors = SafeTensors::deserialize(&buffer).unwrap();
        let layer_norm = LayerNorm::from(
            tensors.tensor("ln_f.weight").unwrap(),
            tensors.tensor("ln_f.bias").unwrap(),
        );
        let data = layer_norm.weight.data();
        assert_eq!(
            simplify(&data[..10]),
            // Values obtained through python
            [1.3971, 1.3750, 1.8870, 1.1688, 1.2724, 1.2508, 9.4198, 1.4371, 1.4527, 1.1856]
        );
        assert_eq!(
            simplify(&data[data.len() - 10..]),
            // Values obtained through python
            [1.1758, 1.4514, 1.1525, 1.1731, 4.2194, 1.1660, 1.1625, 1.1034, 1.0980, 1.2070]
        );

        let weight = ViewTensor::new(&[-1.0, 4.0], vec![2]);
        let bias = ViewTensor::new(&[1.0, 2.0], vec![2]);
        let epsilon = 1e-5;
        let layer_norm = LayerNorm {
            weight,
            bias,
            epsilon,
        };

        let mut input = OwnedTensor::new(vec![10.0, 1.0, 1.0, 1.0], vec![2, 2]);
        layer_norm.forward(&mut input);
        assert_eq!(
            simplify(input.data()),
            // Values obtained through python
            [0.0, -2.0, 1.0, 2.0]
        );
    }

    #[test]
    fn attention_data() {
        let filename = "model.safetensors";
        let file = std::fs::File::open(filename).unwrap();
        let buffer = unsafe { MmapOptions::new().map(&file).unwrap() };
        let tensors = SafeTensors::deserialize(&buffer).unwrap();
        let attention = Attention::from_tensors(0, &tensors, 12);
        let data = attention.c_attn.weight.data();
        assert_eq!(
            simplify(&data[..10]),
            // Values obtained through python
            [
                -0.4738, -0.2614, -0.0978, -0.3499, 0.2243, -0.0429, 0.4187, 0.1744, -0.1883,
                0.1836
            ]
        );
        assert_eq!(
            simplify(&data[data.len() - 10..]),
            // Values obtained through python
            [0.0015, -0.0719, 0.0741, 0.0541, 0.0540, 0.0205, 0.0176, -0.0046, 0.0070, 0.0198]
        );
    }

    #[test]
    fn attention() {
        // Values gotten from Python
        // ```python
        // import torch
        // from transformers.models.gpt2.modeling_gpt2 import GPT2Attention, GPT2Config
        // config = GPT2Config(n_embd=8, n_head=2)
        // attn = GPT2Attention(config)
        // # remove dropout
        // attn.eval()
        // attn.c_attn.weight = torch.nn.Parameter(torch.arange(attn.c_attn.weight.nelement()).view(attn.c_attn.weight.shape).float())
        // attn.c_attn.bias = torch.nn.Parameter(torch.arange(attn.c_attn.bias.nelement()).view(attn.c_attn.bias.shape).float())
        // attn.c_proj.weight = torch.nn.Parameter(torch.arange(attn.c_proj.weight.nelement()).view(attn.c_proj.weight.shape).float())
        // attn.c_proj.bias = torch.nn.Parameter(torch.arange(attn.c_proj.bias.nelement()).view(attn.c_proj.bias.shape).float())
        // input = torch.ones((1, 3, 8))
        // attn_weights, (past_key, past_value) = attn(input, use_cache=True)
        // print(attn_weights.view(-1))
        // print(past_key.shape)
        // print(past_key.reshape(-1))
        // print(past_value.shape)
        // print(past_value.reshape(-1))
        //
        //
        // print()
        // print("Second pass")
        // new_input = torch.ones((1, 1, 8))
        // attn_weights2, (past_key, past_value) = attn(new_input, layer_past = (past_key, past_value), use_cache=True)
        // print(attn_weights2.view(-1))
        // print(past_key.shape)
        // print(past_key.view(-1))
        // print(past_value.shape)
        // print(past_value.view(-1))
        // ```
        let hidden_dim = 8;
        let num_heads = 2;
        let head_dim = hidden_dim / num_heads;
        let data_w = (0..hidden_dim * hidden_dim * 3)
            .map(|i| i as f32)
            .collect::<Vec<_>>();
        let weight = ViewTensor::new(&data_w, vec![hidden_dim, hidden_dim * 3]);
        let data_b = (0..hidden_dim * 3).map(|i| i as f32).collect::<Vec<_>>();
        let bias = ViewTensor::new(&data_b, vec![hidden_dim * 3]);
        let c_attn = Linear::new(weight, bias);

        let data_w2 = (0..hidden_dim * hidden_dim)
            .map(|i| i as f32)
            .collect::<Vec<_>>();
        let weight = ViewTensor::new(&data_w2, vec![hidden_dim, hidden_dim]);
        let data_b2 = (0..hidden_dim).map(|i| i as f32).collect::<Vec<_>>();
        let bias = ViewTensor::new(&data_b2, vec![hidden_dim]);
        let c_proj = Linear::new(weight, bias);

        let attention = Attention {
            c_attn,
            c_proj,
            num_heads,
        };
        let sequence_length = 3;
        let mut input = OwnedTensor::new(
            vec![1.0; hidden_dim * sequence_length],
            vec![sequence_length, hidden_dim],
        );

        let key = OwnedTensor::zeros(vec![num_heads, 0, head_dim]);
        let value = OwnedTensor::zeros(vec![num_heads, 0, head_dim]);
        let mut past = PastKeyValue { key, value };
        attention.forward(&mut input, &mut past);
        assert_eq!(
            input.data(),
            &[
                192864., 199645., 206426., 213207., 219988., 226769., 233550., 240331., 192864.,
                199645., 206426., 213207., 219988., 226769., 233550., 240331., 192864., 199645.,
                206426., 213207., 219988., 226769., 233550., 240331.
            ]
        );

        assert_eq!(past.key.shape(), vec![2, 3, 4]);
        assert_eq!(
            past.key.data(),
            [
                744., 753., 762., 771., 744., 753., 762., 771., 744., 753., 762., 771., 780., 789.,
                798., 807., 780., 789., 798., 807., 780., 789., 798., 807.
            ]
        );
        assert_eq!(past.value.shape(), vec![2, 3, 4]);
        assert_eq!(
            past.value.data(),
            [
                816., 825., 834., 843., 816., 825., 834., 843., 816., 825., 834., 843., 852., 861.,
                870., 879., 852., 861., 870., 879., 852., 861., 870., 879.
            ]
        );

        // Second pass
        let sequence_length = 1;
        let mut input = OwnedTensor::new(vec![1.0; hidden_dim], vec![sequence_length, hidden_dim]);
        attention.forward(&mut input, &mut past);
        assert_eq!(
            input.data(),
            &[192864., 199645., 206426., 213207., 219988., 226769., 233550., 240331.]
        );
        assert_eq!(past.key.shape(), vec![2, 4, 4]);
        assert_eq!(
            past.key.data(),
            &[
                744., 753., 762., 771., 744., 753., 762., 771., 744., 753., 762., 771., 744., 753.,
                762., 771., 780., 789., 798., 807., 780., 789., 798., 807., 780., 789., 798., 807.,
                780., 789., 798., 807.
            ]
        );
        assert_eq!(past.value.shape(), vec![2, 4, 4]);
        assert_eq!(
            past.value.data(),
            &[
                816., 825., 834., 843., 816., 825., 834., 843., 816., 825., 834., 843., 816., 825.,
                834., 843., 852., 861., 870., 879., 852., 861., 870., 879., 852., 861., 870., 879.,
                852., 861., 870., 879.
            ]
        );
    }

    #[test]
    fn mlp() {
        let hidden_dim = 8;
        let data = (0..hidden_dim * hidden_dim * 4)
            .map(|i| i as f32)
            .collect::<Vec<_>>();
        let weight = ViewTensor::new(&data, vec![hidden_dim, hidden_dim * 4]);
        let data = (0..hidden_dim * 4).map(|i| i as f32).collect::<Vec<_>>();
        let bias = ViewTensor::new(&data, vec![hidden_dim * 4]);
        let c_fc = Linear::new(weight, bias);

        let data = (0..hidden_dim * hidden_dim * 4)
            .map(|i| i as f32)
            .collect::<Vec<_>>();
        let weight = ViewTensor::new(&data, vec![hidden_dim * 4, hidden_dim]);
        let data = (0..hidden_dim).map(|i| i as f32).collect::<Vec<_>>();
        let bias = ViewTensor::new(&data, vec![hidden_dim]);
        let c_proj = Linear::new(weight, bias);

        let mlp = Mlp { c_fc, c_proj };
        let mut input = OwnedTensor::new(vec![1.0; hidden_dim], vec![1, hidden_dim]);
        mlp.forward(&mut input);
        assert_eq!(
            input.data(),
            // Values gotten from Python
            // ```python
            // import torch
            // from transformers.models.gpt2.modeling_gpt2 import GPT2MLP, GPT2Config
            // config = GPT2Config(n_embd=8, n_head=2, activation_function="gelu_new")
            // mlp = GPT2MLP(config=config, intermediate_size = config.n_embd * 4)
            // # remove dropout
            // mlp.eval()
            // mlp.c_fc.weight = torch.nn.Parameter(torch.arange(mlp.c_fc.weight.nelement()).view(mlp.c_fc.weight.shape).float())
            // mlp.c_fc.bias = torch.nn.Parameter(torch.arange(mlp.c_fc.bias.nelement()).view(mlp.c_fc.bias.shape).float())
            // mlp.c_proj.weight = torch.nn.Parameter(torch.arange(mlp.c_proj.weight.nelement()).view(mlp.c_proj.weight.shape).float())
            // mlp.c_proj.bias = torch.nn.Parameter(torch.arange(mlp.c_proj.bias.nelement()).view(mlp.c_proj.bias.shape).float())
            // input = torch.ones((1, 1, 8))
            // print(mlp(input)[0].view(-1))
            // ```
            &[4305280., 4338417., 4371554., 4404691., 4437828., 4470965., 4504102., 4537239.]
        );
    }
}
