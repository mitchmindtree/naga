[[block]]
struct PrimeIndices {
    indices: [[stride(4)]] array<u32>;
};

[[group(0), binding(0)]]
var<storage, read_write> global: PrimeIndices;
var<private> gl_GlobalInvocationID: vec3<u32>;

fn collatz_iterations(n: u32) -> u32 {
    var n1: u32;
    var i: u32 = 0u;

    n1 = n;
    loop {
        let _e7: u32 = n1;
        if (!((_e7 != u32(1)))) {
            break;
        }
        {
            let _e14: u32 = n1;
            if (((f32(_e14) % f32(2)) == f32(0))) {
                {
                    let _e22: u32 = n1;
                    n1 = (_e22 / u32(2));
                }
            } else {
                {
                    let _e27: u32 = n1;
                    n1 = ((u32(3) * _e27) + u32(1));
                }
            }
            let _e33: u32 = i;
            i = (_e33 + 1u);
        }
    }
    let _e36: u32 = i;
    return _e36;
}

fn main1() {
    var index: u32;

    let _e3: vec3<u32> = gl_GlobalInvocationID;
    index = _e3.x;
    let _e6: u32 = index;
    let _e8: u32 = index;
    let _e11: u32 = index;
    let _e13: u32 = global.indices[_e11];
    let _e14: u32 = collatz_iterations(_e13);
    global.indices[_e6] = _e14;
    return;
}

[[stage(compute), workgroup_size(1, 1, 1)]]
fn main([[builtin(global_invocation_id)]] param: vec3<u32>) {
    gl_GlobalInvocationID = param;
    main1();
    return;
}
