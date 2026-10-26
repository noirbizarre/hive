<script>
import MarkdownIt from 'markdown-it';

// Only parser-generated HTML crosses v-html: raw HTML stays literal and the default URL validator rejects executable links.
const markdown = new MarkdownIt({ html: false, breaks: true });
</script>

<script setup>
import { computed } from 'vue';

const props = defineProps({ text: { type: String, required: true } });
// Reparse accumulated text so incomplete streaming delimiters can close, without reparsing unchanged messages.
const rendered = computed(() => markdown.render(props.text));
</script>

<template>
  <div class="message-text markdown-message" v-html="rendered"></div>
</template>
