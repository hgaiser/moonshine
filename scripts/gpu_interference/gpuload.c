// Headless, deterministic GPU-bound "game" used to measure streaming interference.
// Renders a fixed-cost full-screen pass (texture bandwidth + ALU) into an
// offscreen RGBA16F target on the graphics queue with two frames in flight,
// like a vsync-off game. Reports achieved FPS, frame-interval percentiles,
// 1% low FPS and per-frame GPU time (timestamps include time-slicing with
// other processes, so they also expose contention).
//
// Usage: gpuload [--width W] [--height H] [--iters N] [--seconds S]
//                [--warmup S] [--fps CAP] [--json PATH]
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include "shaders.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s:%d %s = %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)
#define FIF 2

static double now_s(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec * 1e-9;
}

static int cmp_d(const void *a, const void *b) {
	double x = *(const double *)a, y = *(const double *)b;
	return x < y ? -1 : x > y;
}

static uint32_t find_mem(VkPhysicalDevice pd, uint32_t bits, VkMemoryPropertyFlags want) {
	VkPhysicalDeviceMemoryProperties mp;
	vkGetPhysicalDeviceMemoryProperties(pd, &mp);
	for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
		if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
			return i;
	fprintf(stderr, "no memory type\n");
	exit(1);
}

static VkImage make_image(VkDevice dev, VkPhysicalDevice pd, uint32_t w, uint32_t h, VkImageUsageFlags usage, VkDeviceMemory *mem) {
	VkImageCreateInfo ci = {
		.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
		.format = VK_FORMAT_R16G16B16A16_SFLOAT, .extent = {w, h, 1}, .mipLevels = 1, .arrayLayers = 1,
		.samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL, .usage = usage,
		.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
	};
	VkImage img;
	CHECK(vkCreateImage(dev, &ci, NULL, &img));
	VkMemoryRequirements req;
	vkGetImageMemoryRequirements(dev, img, &req);
	VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size,
		.memoryTypeIndex = find_mem(pd, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)};
	CHECK(vkAllocateMemory(dev, &ai, NULL, mem));
	CHECK(vkBindImageMemory(dev, img, *mem, 0));
	return img;
}

static VkImageView make_view(VkDevice dev, VkImage img) {
	VkImageViewCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = img,
		.viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R16G16B16A16_SFLOAT,
		.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
	VkImageView v;
	CHECK(vkCreateImageView(dev, &ci, NULL, &v));
	return v;
}

int main(int argc, char **argv) {
	uint32_t W = 3840, H = 2160, iters = 64, taps = 4;
	double seconds = 20, warmup = 3, fps_cap = 0;
	const char *json = NULL;
	for (int i = 1; i + 1 < argc; i += 2) {
		if (!strcmp(argv[i], "--width")) W = atoi(argv[i + 1]);
		else if (!strcmp(argv[i], "--height")) H = atoi(argv[i + 1]);
		else if (!strcmp(argv[i], "--iters")) iters = atoi(argv[i + 1]);
		else if (!strcmp(argv[i], "--taps")) taps = atoi(argv[i + 1]);
		else if (!strcmp(argv[i], "--seconds")) seconds = atof(argv[i + 1]);
		else if (!strcmp(argv[i], "--warmup")) warmup = atof(argv[i + 1]);
		else if (!strcmp(argv[i], "--fps")) fps_cap = atof(argv[i + 1]);
		else if (!strcmp(argv[i], "--json")) json = argv[i + 1];
		else { fprintf(stderr, "unknown arg %s\n", argv[i]); return 2; }
	}

	VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "gpuload", .apiVersion = VK_API_VERSION_1_3};
	VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
	VkInstance inst;
	CHECK(vkCreateInstance(&ici, NULL, &inst));
	uint32_t npd = 8;
	VkPhysicalDevice pds[8];
	CHECK(vkEnumeratePhysicalDevices(inst, &npd, pds));
	VkPhysicalDevice pd = VK_NULL_HANDLE;
	VkPhysicalDeviceProperties props;
	for (uint32_t i = 0; i < npd; i++) {
		vkGetPhysicalDeviceProperties(pds[i], &props);
		if (props.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) { pd = pds[i]; break; }
	}
	if (!pd) { pd = pds[0]; vkGetPhysicalDeviceProperties(pd, &props); }
	fprintf(stderr, "gpuload: %s %ux%u iters=%u\n", props.deviceName, W, H, iters);

	uint32_t nq = 16;
	VkQueueFamilyProperties qf[16];
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
	uint32_t qfi = 0;
	for (; qfi < nq; qfi++)
		if (qf[qfi].queueFlags & VK_QUEUE_GRAPHICS_BIT) break;
	float prio = 1.0f;
	VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = qfi, .queueCount = 1, .pQueuePriorities = &prio};
	VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
	VkDevice dev;
	CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
	VkQueue q;
	vkGetDeviceQueue(dev, qfi, 0, &q);

	VkCommandPoolCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = qfi};
	VkCommandPool pool;
	CHECK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
	VkCommandBuffer cbs[FIF + 1];
	VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = FIF + 1};
	CHECK(vkAllocateCommandBuffers(dev, &cbai, cbs));

	// Source texture with incompressible noise so DCC cannot hide bandwidth.
	VkDeviceMemory tex_mem;
	VkImage tex = make_image(dev, pd, W, H, VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT, &tex_mem);
	VkDeviceSize tex_bytes = (VkDeviceSize)W * H * 8;
	VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = tex_bytes, .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT};
	VkBuffer staging;
	CHECK(vkCreateBuffer(dev, &bci, NULL, &staging));
	VkMemoryRequirements sreq;
	vkGetBufferMemoryRequirements(dev, staging, &sreq);
	VkMemoryAllocateInfo sai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = sreq.size,
		.memoryTypeIndex = find_mem(pd, sreq.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)};
	VkDeviceMemory smem;
	CHECK(vkAllocateMemory(dev, &sai, NULL, &smem));
	CHECK(vkBindBufferMemory(dev, staging, smem, 0));
	uint16_t *p;
	CHECK(vkMapMemory(dev, smem, 0, tex_bytes, 0, (void **)&p));
	uint32_t seed = 12345;
	for (VkDeviceSize i = 0; i < tex_bytes / 2; i++) {
		seed = seed * 1664525u + 1013904223u;
		p[i] = 0x3800 | ((seed >> 16) & 0x3ff); // half floats in [0.5, 1)
	}
	vkUnmapMemory(dev, smem);

	VkDeviceMemory rt_mem[FIF];
	VkImage rt[FIF];
	VkImageView rtv[FIF];
	for (int i = 0; i < FIF; i++) {
		rt[i] = make_image(dev, pd, W, H, VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, &rt_mem[i]);
		rtv[i] = make_view(dev, rt[i]);
	}
	VkImageView texv = make_view(dev, tex);

	// Upload.
	VkCommandBufferBeginInfo cbbi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
	CHECK(vkBeginCommandBuffer(cbs[FIF], &cbbi));
	VkImageMemoryBarrier b = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
		.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
		.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
		.image = tex, .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
	vkCmdPipelineBarrier(cbs[FIF], VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b);
	VkBufferImageCopy reg = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1}, .imageExtent = {W, H, 1}};
	vkCmdCopyBufferToImage(cbs[FIF], staging, tex, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &reg);
	b.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
	b.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
	b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
	b.newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
	vkCmdPipelineBarrier(cbs[FIF], VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL, 0, NULL, 1, &b);
	CHECK(vkEndCommandBuffer(cbs[FIF]));
	VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cbs[FIF]};
	CHECK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));
	CHECK(vkQueueWaitIdle(q));

	// Pipeline.
	VkAttachmentDescription att = {.format = VK_FORMAT_R16G16B16A16_SFLOAT, .samples = VK_SAMPLE_COUNT_1_BIT,
		.loadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
		.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE, .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
		.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED, .finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
	VkAttachmentReference ar = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
	VkSubpassDescription sp = {.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS, .colorAttachmentCount = 1, .pColorAttachments = &ar};
	VkRenderPassCreateInfo rpci = {.sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, .attachmentCount = 1, .pAttachments = &att, .subpassCount = 1, .pSubpasses = &sp};
	VkRenderPass rp;
	CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));
	VkFramebuffer fb[FIF];
	for (int i = 0; i < FIF; i++) {
		VkFramebufferCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, .renderPass = rp, .attachmentCount = 1, .pAttachments = &rtv[i], .width = W, .height = H, .layers = 1};
		CHECK(vkCreateFramebuffer(dev, &fci, NULL, &fb[i]));
	}
	VkSamplerCreateInfo sci = {.sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO, .magFilter = VK_FILTER_LINEAR, .minFilter = VK_FILTER_LINEAR,
		.addressModeU = VK_SAMPLER_ADDRESS_MODE_REPEAT, .addressModeV = VK_SAMPLER_ADDRESS_MODE_REPEAT, .addressModeW = VK_SAMPLER_ADDRESS_MODE_REPEAT, .maxLod = 0};
	VkSampler smp;
	CHECK(vkCreateSampler(dev, &sci, NULL, &smp));
	VkDescriptorSetLayoutBinding dslb = {0, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_FRAGMENT_BIT, NULL};
	VkDescriptorSetLayoutCreateInfo dslci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &dslb};
	VkDescriptorSetLayout dsl;
	CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
	VkPushConstantRange pcr = {VK_SHADER_STAGE_FRAGMENT_BIT, 0, 12};
	VkPipelineLayoutCreateInfo plci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &dsl, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr};
	VkPipelineLayout pl;
	CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
	VkDescriptorPoolSize dps = {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1};
	VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &dps};
	VkDescriptorPool dp;
	CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));
	VkDescriptorSetAllocateInfo dsai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl};
	VkDescriptorSet ds;
	CHECK(vkAllocateDescriptorSets(dev, &dsai, &ds));
	VkDescriptorImageInfo dii = {smp, texv, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
	VkWriteDescriptorSet wds = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, .pImageInfo = &dii};
	vkUpdateDescriptorSets(dev, 1, &wds, 0, NULL);

	VkShaderModule vsm, fsm;
	VkShaderModuleCreateInfo smci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = vs_spv_len, .pCode = (const uint32_t *)vs_spv};
	CHECK(vkCreateShaderModule(dev, &smci, NULL, &vsm));
	smci.codeSize = fs_spv_len;
	smci.pCode = (const uint32_t *)fs_spv;
	CHECK(vkCreateShaderModule(dev, &smci, NULL, &fsm));
	VkPipelineShaderStageCreateInfo st[2] = {
		{.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vsm, .pName = "main"},
		{.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fsm, .pName = "main"}};
	VkPipelineVertexInputStateCreateInfo vi = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
	VkPipelineInputAssemblyStateCreateInfo ia = {.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
	VkViewport vp = {0, 0, (float)W, (float)H, 0, 1};
	VkRect2D sc = {{0, 0}, {W, H}};
	VkPipelineViewportStateCreateInfo vps = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc};
	VkPipelineRasterizationStateCreateInfo rs = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO, .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1};
	VkPipelineMultisampleStateCreateInfo ms = {.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
	VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
	VkPipelineColorBlendStateCreateInfo cb = {.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, .attachmentCount = 1, .pAttachments = &cba};
	VkGraphicsPipelineCreateInfo gpci = {.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .stageCount = 2, .pStages = st,
		.pVertexInputState = &vi, .pInputAssemblyState = &ia, .pViewportState = &vps, .pRasterizationState = &rs,
		.pMultisampleState = &ms, .pColorBlendState = &cb, .layout = pl, .renderPass = rp};
	VkPipeline pipe;
	CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

	VkQueryPoolCreateInfo qpci = {.sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 2 * FIF};
	VkQueryPool qp;
	CHECK(vkCreateQueryPool(dev, &qpci, NULL, &qp));
	VkFence fences[FIF];
	VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, .flags = VK_FENCE_CREATE_SIGNALED_BIT};
	for (int i = 0; i < FIF; i++)
		CHECK(vkCreateFence(dev, &fci, NULL, &fences[i]));
	int pending[FIF] = {0};

	size_t cap = (size_t)((seconds + 1) * 2000);
	double *intervals = malloc(cap * sizeof(double)), *gpu_ms = malloc(cap * sizeof(double));
	size_t n = 0, ng = 0;
	double t_start = now_s(), t_measure = t_start + warmup, t_end = t_measure + seconds;
	double last_done = 0, next_deadline = t_start;
	uint64_t frame = 0;
	int measuring = 0;
	double measure_began = 0;
	uint64_t measured_frames = 0;

	for (;;) {
		int i = frame % FIF;
		CHECK(vkWaitForFences(dev, 1, &fences[i], VK_TRUE, UINT64_MAX));
		double t = now_s();
		if (pending[i]) {
			uint64_t ts[2];
			if (vkGetQueryPoolResults(dev, qp, 2 * i, 2, sizeof ts, ts, 8, VK_QUERY_RESULT_64_BIT) == VK_SUCCESS && measuring && ng < cap)
				gpu_ms[ng++] = (ts[1] - ts[0]) * props.limits.timestampPeriod * 1e-6;
			if (measuring) {
				if (n < cap && last_done > 0) intervals[n++] = (t - last_done) * 1e3;
				measured_frames++;
			}
			last_done = t;
		}
		if (!measuring && t >= t_measure) { measuring = 1; measure_began = t; last_done = t; }
		if (t >= t_end) break;
		if (fps_cap > 0) {
			next_deadline += 1.0 / fps_cap;
			double d = next_deadline - now_s();
			if (d > 0) { struct timespec ts = {(time_t)d, (long)((d - (time_t)d) * 1e9)}; nanosleep(&ts, NULL); }
			else next_deadline = now_s();
		}
		CHECK(vkResetFences(dev, 1, &fences[i]));
		VkCommandBuffer c = cbs[i];
		CHECK(vkBeginCommandBuffer(c, &cbbi));
		vkCmdResetQueryPool(c, qp, 2 * i, 2);
		vkCmdWriteTimestamp(c, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qp, 2 * i);
		VkRenderPassBeginInfo rpbi = {.sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = rp, .framebuffer = fb[i], .renderArea = sc};
		vkCmdBeginRenderPass(c, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
		vkCmdBindPipeline(c, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
		vkCmdBindDescriptorSets(c, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
		uint32_t pc[3] = {iters, (uint32_t)frame, taps};
		vkCmdPushConstants(c, pl, VK_SHADER_STAGE_FRAGMENT_BIT, 0, 12, pc);
		vkCmdDraw(c, 3, 1, 0, 0);
		vkCmdEndRenderPass(c);
		vkCmdWriteTimestamp(c, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 2 * i + 1);
		CHECK(vkEndCommandBuffer(c));
		VkSubmitInfo s = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &c};
		CHECK(vkQueueSubmit(q, 1, &s, fences[i]));
		pending[i] = 1;
		frame++;
	}
	double elapsed = last_done - measure_began;
	vkDeviceWaitIdle(dev);

	qsort(intervals, n, sizeof(double), cmp_d);
	qsort(gpu_ms, ng, sizeof(double), cmp_d);
	double sum = 0, sum2 = 0, gsum = 0;
	for (size_t k = 0; k < n; k++) { sum += intervals[k]; sum2 += intervals[k] * intervals[k]; }
	for (size_t k = 0; k < ng; k++) gsum += gpu_ms[k];
	double mean = n ? sum / n : 0, var = n ? sum2 / n - mean * mean : 0;
	size_t worst = n / 100 ? n / 100 : 1;
	double wsum = 0;
	for (size_t k = n - worst; k < n; k++) wsum += intervals[k];
#define PCT(a, cnt, pp) ((cnt) ? (a)[(size_t)((cnt - 1) * (pp))] : 0)
	char out[2048];
	snprintf(out, sizeof out,
		"{\"width\":%u,\"height\":%u,\"iters\":%u,\"taps\":%u,\"fps_cap\":%.1f,\"frames\":%lu,\"seconds\":%.3f,\"fps\":%.3f,"
		"\"frametime_ms\":{\"mean\":%.4f,\"p50\":%.4f,\"p95\":%.4f,\"p99\":%.4f,\"p999\":%.4f,\"max\":%.4f,\"stddev\":%.4f},"
		"\"one_percent_low_fps\":%.3f,"
		"\"gpu_ms\":{\"mean\":%.4f,\"p50\":%.4f,\"p95\":%.4f,\"p99\":%.4f,\"max\":%.4f}}\n",
		W, H, iters, taps, fps_cap, (unsigned long)measured_frames, elapsed, measured_frames / elapsed,
		mean, PCT(intervals, n, 0.5), PCT(intervals, n, 0.95), PCT(intervals, n, 0.99), PCT(intervals, n, 0.999),
		n ? intervals[n - 1] : 0, var > 0 ? __builtin_sqrt(var) : 0,
		worst ? 1000.0 / (wsum / worst) : 0,
		ng ? gsum / ng : 0, PCT(gpu_ms, ng, 0.5), PCT(gpu_ms, ng, 0.95), PCT(gpu_ms, ng, 0.99), ng ? gpu_ms[ng - 1] : 0);
	fputs(out, stdout);
	if (json) { FILE *f = fopen(json, "w"); if (f) { fputs(out, f); fclose(f); } }
	return 0;
}
