import { mount, View } from "nsh";
import { ShimmerText } from "./app/components/shimmer-text";

mount(() => (
  <View class="h-full w-full gap-6 p-6">
    <ShimmerText testID="shimmer-active" duration={1200} bandWidth={48} highlightColor="#60a5fa" class="self-start">
      Thinking
    </ShimmerText>
    <ShimmerText testID="shimmer-inactive" active={false} class="self-start">
      Static text
    </ShimmerText>
  </View>
));
